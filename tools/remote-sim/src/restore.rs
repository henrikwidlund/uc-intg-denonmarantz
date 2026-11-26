// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Saves the receiver settings that the `simple_commands` check changes, and the source (see
//! [`mark_source`]), and sets them back. The saved lines are also written to a file. If a run stops
//! before the settings are set back, the next run sets them back at its start.

use crate::oracle::Oracle;
use crate::{Fail, Options, checks};
use denon_marantz::proto::telnet::EventZone;
use denon_marantz::{Ask, Avr, Command, Zone};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

/// The file with the saved settings, in the repository folder. It exists only while settings are
/// changed.
pub static FILE: LazyLock<PathBuf> =
    LazyLock::new(|| crate::repo_root().join("remote-sim-restore.txt"));

/// Queries of the settings, in the order in which they are set back. The source goes first: some
/// settings are kept for each source. The sound mode goes next: it can change the sound settings.
/// The speaker preset goes before the Dirac filter: each preset has its own filter. The Dirac
/// filter goes before MultEQ, and the tone control before bass and treble. [`send_order`] changes
/// this order for bass and treble, and [`write`] sends a source and a speaker preset that differ
/// first. A caller that does not set the source back marks the source line with [`mark_source`].
pub const QUERIES: &[&str] = &[
    "SI?",
    "MS?",
    "VSAUDIO ?",
    "VSMONI ?",
    "VSVPM ?",
    "BTTX ?",
    "BTLEV ?",
    "SLP?",
    "MNZST?",
    "ECO?",
    "SPPR ?",
    "PSDIRAC ?",
    "PSMULTEQ: ?",
    "PSDYNEQ ?",
    "PSREFLEV ?",
    "PSDYNVOL ?",
    "PSLFC ?",
    "PSDRC ?",
    "PSRSTR ?",
    "PSGEQ ?",
    "PSHEQ ?",
    "PSCINEMA EQ. ?",
    "PSCES ?",
    "PSLOM ?",
    "PSSPV ?",
    "PSSP: ?",
    "PSFH: ?",
    "PSPAN ?",
    "PSSURLEV ?",
    "PSIMAX ?",
    "PSIMAXAUD ?",
    "PSIMAXHPF ?",
    "PSIMAXLPF ?",
    "PSIMAXSWM ?",
    "PSIMAXSWO ?",
    "PSAUROST ?",
    "PSAUROMODE ?",
    "PSAUROPR ?",
    "PSDIC ?",
    "PSDELAY ?",
    "PSDEL ?",
    "PSRSZ ?",
    "PSTONE CTRL ?",
    "PSBAS ?",
    "PSTRE ?",
    // The receiver takes the tactile channel only while the transducer is on.
    "CVTTR ?",
    "SSTTR ?",
    "SSTTRLEV ?",
    "SSTTRLPF ?",
    "CV?",
];

/// The longest wait for the answers. The receiver answers late while other queries of the
/// connection wait before them.
const ANSWER_LIMIT: Duration = Duration::from_secs(15);

/// The read ends when no answer came for this long after the answers started. The receiver does
/// not answer some queries in some sound modes, for example `PSPAN ?`.
const ANSWER_GAP: Duration = Duration::from_secs(2);

/// How long more lines are collected after each query has an answer. Some queries have more than
/// one answer line.
const LAST_LINES: Duration = Duration::from_millis(300);

/// The answer lines of the receiver to `queries`, a part of [`QUERIES`], in the order of
/// [`QUERIES`]. Each answer is also the command that sets the value again.
pub async fn read(avr: &Avr, queries: &[&str]) -> Result<Vec<String>, Fail> {
    let mut events = avr.telnet_events();
    let asks: Vec<Ask<'_>> = queries.iter().map(|q| Ask::Raw(q)).collect();
    avr.telnet_ask(&asks).await?;
    let mut waiting: Vec<&str> = queries.to_vec();
    let mut lines: Vec<String> = Vec::new();
    let limit = tokio::time::Instant::now() + ANSWER_LIMIT;
    let mut end = limit;
    loop {
        let event = match tokio::time::timeout_at(end, events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) | Err(_) => break,
        };
        if !for_main_zone(event.zone) {
            continue;
        }
        let line = format!("{}{}", event.event.code(), event.parameter);
        let Some(query) = position(&line).map(|i| QUERIES[i]) else {
            continue;
        };
        if !queries.contains(&query) {
            continue;
        }
        let now = tokio::time::Instant::now();
        end = limit.min(now + ANSWER_GAP);
        // The channel levels end with `CVEND`.
        if query != "CV?" || line == "CVEND" {
            waiting.retain(|q| *q != query);
            if waiting.is_empty() {
                end = end.min(now + LAST_LINES);
            }
        }
        if is_saved(&line) && !lines.contains(&line) {
            lines.push(line);
        }
    }
    lines.sort_by_key(|line| position(line));
    Ok(lines)
}

/// The line is about the main zone or all zones. The lines of the other zones look like main zone
/// lines, for example `Z2CD` as `SICD`. All Zone Stereo (`MNZST`) is a line for all zones.
fn for_main_zone(zone: EventZone) -> bool {
    zone.applies_to(Zone::Main)
}

/// The place of the query that a line answers, or `None` if no query asks for it. The longest
/// query wins: `SSTTRLEV 50` answers `SSTTRLEV ?`, not `SSTTR ?`.
fn position(line: &str) -> Option<usize> {
    QUERIES
        .iter()
        .enumerate()
        .map(|(i, q)| (i, q.trim_end_matches('?').trim_end()))
        .filter(|(_, prefix)| line.starts_with(prefix))
        .max_by_key(|(_, prefix)| prefix.len())
        .map(|(i, _)| i)
}

/// Lines that set a value. `CVEND` ends the list of channel levels, and `MSQUICK` is a quick
/// select memory: both are not settings.
fn is_saved(line: &str) -> bool {
    position(line).is_some() && line != "CVEND" && !line.starts_with("MSQUICK")
}

/// Send the saved lines that differ from the receiver, read with `queries`. Lines that did not come
/// back are sent once more. A source and a speaker preset that differ are sent first, each alone,
/// and the receiver is read again after each. Nothing is sent, or nothing more, if the source
/// differs and the main zone is not on, if the receiver did not take the source or does not report
/// it later, or if another source than a marked one (see [`mark_source`]) is selected or there is
/// more than one marked line. Then all lines are returned. Else returns the lines that the receiver
/// did not take: they have a different value when read again.
pub async fn write(avr: &Avr, queries: &[&str], lines: &[String]) -> Result<Vec<String>, Fail> {
    let marked = lines.iter().any(|l| l.starts_with(MARKED_SOURCE));
    let start = single_code(lines, MARKED_SOURCE);
    let lines: Vec<String> = lines
        .iter()
        .filter(|l| !l.starts_with(MARKED_SOURCE))
        .cloned()
        .collect();
    let lines = lines.as_slice();
    // Only the lines that differ are sent, so nothing changes that does not need to.
    let mut now = read(avr, queries).await?;
    // The marked source is not set back, so nothing is sent while another source is selected.
    if marked && !start.is_some_and(|code| single_code(&now, SOURCE) == Some(code)) {
        return Ok(lines.to_vec());
    }
    // Some settings are kept for each source, and a speaker preset changes the settings of the
    // preset. So both go first and alone, and the other lines are compared with the values of the
    // saved source and preset.
    for prefix in [SOURCE, PRESET] {
        if let Some(line) = lines
            .iter()
            .find(|line| line.starts_with(prefix) && !taken(&now, line))
        {
            // A source command can switch the zone on, at a volume that nothing lowers here.
            if prefix == SOURCE
                && avr.snapshot().zone(Zone::Main).power != Some(denon_marantz::proto::Power::On)
            {
                return Ok(lines.to_vec());
            }
            send(avr, std::slice::from_ref(line)).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            now = read(avr, queries).await?;
            // Without the saved source, the other lines would change the settings of another
            // source.
            if prefix == SOURCE && !taken(&now, line) {
                return Ok(lines.to_vec());
            }
        }
    }
    let changed = with_detour(lines, differing(lines, &now));
    if changed.is_empty() {
        return Ok(changed);
    }
    // A source line is sent only alone, after the power check above: here the receiver lost the
    // source again, or did not report it.
    if changed.iter().any(|l| l.starts_with(SOURCE)) {
        return Ok(lines.to_vec());
    }
    send(avr, &changed).await?;
    // The receiver needs time to apply a source, a sound mode or a speaker preset.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let now = read(avr, queries).await?;
    let missed = differing(lines, &now);
    if missed.is_empty() {
        return Ok(missed);
    }
    // A busy receiver can answer too late, or ignore a line while it applies the source, the sound
    // mode or a speaker preset. Send the missed lines again and ask for them only. The Dirac and
    // tone control lines are checked again too, because the retry changes them. A Dirac change can
    // change MultEQ and Dynamic EQ, and a preset change can change all settings. A source line is
    // sent only alone, after the power check above.
    if missed.iter().any(|l| l.starts_with(SOURCE)) {
        return Ok(lines.to_vec());
    }
    let missed = with_detour(lines, missed);
    send(avr, &missed).await?;
    let slow = missed
        .iter()
        .any(|l| l.starts_with(SOUND_MODE) || l.starts_with(PRESET));
    tokio::time::sleep(Duration::from_secs(if slow { 2 } else { 1 })).await;
    let mut again: Vec<&str> = Vec::new();
    let mut retry: Vec<&str> = missed
        .iter()
        .filter_map(|line| position(line))
        .map(|i| QUERIES[i])
        .collect();
    if missed.iter().any(|line| line.starts_with(DIRAC)) {
        retry.extend(DIRAC_DEPENDENT);
    }
    // A preset change can change all settings.
    if missed.iter().any(|line| line.starts_with(PRESET)) {
        retry.extend(queries);
    }
    for query in retry {
        if !again.contains(&query) {
            again.push(query);
        }
    }
    let now = read(avr, &again).await?;
    Ok(lines
        .iter()
        .filter(|line| position(line).is_some_and(|i| again.contains(&QUERIES[i])))
        .filter(|line| !taken(&now, line))
        .cloned()
        .collect())
}

fn differing(lines: &[String], now: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| !taken(now, line))
        .cloned()
        .collect()
}

/// Adds the saved Dirac and tone control lines when a bass or treble line is sent: [`send_order`]
/// needs them.
fn with_detour(lines: &[String], mut list: Vec<String>) -> Vec<String> {
    if list.iter().any(|line| is_tone_level(line)) {
        for prefix in [TONE, DIRAC] {
            if let Some(line) = lines.iter().find(|line| line.starts_with(prefix))
                && !list.contains(line)
            {
                list.push(line.clone());
            }
        }
        list.sort_by_key(|line| position(line));
    }
    list
}

async fn send(avr: &Avr, lines: &[String]) -> Result<(), Fail> {
    let order = send_order(lines);
    for (i, line) in order.iter().enumerate() {
        avr.telnet_send(Zone::Main, Command::Raw(line)).await?;
        let rest = &order[i + 1..];
        // The receiver may reset the sound settings while it applies a source, a sound mode or a
        // speaker preset.
        if [SOURCE, SOUND_MODE, PRESET]
            .iter()
            .any(|p| line.starts_with(p))
            && !rest.is_empty()
        {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        // The receiver may ignore tone control while it changes the Dirac filter.
        if line.starts_with(DIRAC)
            && rest
                .iter()
                .any(|line| is_tone_level(line) || line.starts_with(TONE))
        {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    Ok(())
}

/// The lines in the order to send them. The receiver ignores bass and treble while tone control is
/// off, and tone control while a Dirac filter is selected. So both are first set to a value that
/// allows bass and treble, and set to their saved values after them.
fn send_order(lines: &[String]) -> Vec<&str> {
    let mut order: Vec<&str> = lines.iter().map(String::as_str).collect();
    let Some(last) = order.iter().rposition(|line| is_tone_level(line)) else {
        return order;
    };
    let mut after = Vec::new();
    for (prefix, allow) in [(DIRAC, DIRAC_OFF), (TONE, TONE_ON)] {
        if let Some(i) = order.iter().position(|line| line.starts_with(prefix))
            && i < last
            && order[i] != allow
        {
            after.push(order[i]);
            order[i] = allow;
        }
    }
    // Tone control before Dirac: tone control cannot be changed while a filter is selected.
    after.reverse();
    order.splice(last + 1..last + 1, after);
    order
}

fn is_tone_level(line: &str) -> bool {
    line.starts_with("PSBAS") || line.starts_with("PSTRE")
}

const SOURCE: &str = "SI";
const SOUND_MODE: &str = "MS";
const PRESET: &str = "SPPR";
const DIRAC: &str = "PSDIRAC";
/// Queries of the values that a Dirac change can change.
const DIRAC_DEPENDENT: &[&str] = &["PSMULTEQ: ?", "PSDYNEQ ?"];
const DIRAC_OFF: &str = "PSDIRAC OFF";
const TONE: &str = "PSTONE CTRL";
const TONE_ON: &str = "PSTONE CTRL ON";

/// Start of a line that holds the start source but is never sent.
const MARKED_SOURCE: &str = "#SI";

/// Mark the source lines: [`write`] never sends them, but sends no other line while another source
/// is selected. Some settings are kept for each source.
pub fn mark_source(lines: &mut [String]) {
    for line in lines.iter_mut().filter(|l| l.starts_with(SOURCE)) {
        line.insert(0, '#');
    }
}

/// The receiver has `line`. A source line counts only if the receiver gave exactly one source: with
/// two, the source changed during the read.
fn taken(now: &[String], line: &str) -> bool {
    match line.strip_prefix(SOURCE) {
        Some(code) => single_code(now, SOURCE) == Some(code),
        None => now.iter().any(|l| l == line),
    }
}

/// Skip a check that saves the settings when the start source is not known: without it, the
/// settings could be set back on another source.
pub fn ensure_source(lines: &[String]) -> Result<(), Fail> {
    if saved_source(lines).is_none() {
        return Err(Fail::skip(format!(
            "the start source is not known. Read: {lines:?}"
        )));
    }
    Ok(())
}

/// The source of the saved lines. `None` if there is no source line, or more than one: the source
/// changed during the read, and none of them is known to be the start source.
pub fn saved_source(lines: &[String]) -> Option<&str> {
    single_code(lines, SOURCE)
}

/// The code of the only line that starts with `prefix`. `None` if there is no such line, or more
/// than one.
fn single_code<'a>(lines: &'a [String], prefix: &str) -> Option<&'a str> {
    let mut codes = lines.iter().filter_map(|l| l.strip_prefix(prefix));
    match (codes.next(), codes.next()) {
        (Some(code), None) => Some(code),
        _ => None,
    }
}

/// Write the saved lines to [`FILE`].
pub async fn save(lines: &[String]) -> Result<(), Fail> {
    tokio::fs::write(&*FILE, lines.join("\n") + "\n")
        .await
        .map_err(|e| Fail::new(format!("{}: {e}", FILE.display())))
}

/// Skip a check that saves the settings while [`FILE`] still holds the settings of an earlier run:
/// the check would replace them.
pub fn ensure_nothing_left() -> Result<(), Fail> {
    if FILE.exists() {
        return Err(Fail::skip(format!(
            "settings of the last run are not set back. They are in {}",
            FILE.display()
        )));
    }
    Ok(())
}

/// Remove [`FILE`] after all settings are set back.
pub async fn forget() {
    let _ = tokio::fs::remove_file(&*FILE).await;
}

/// Set back the settings of a run that stopped early. Nothing is sent and the file stays if
/// [`source_refused`] gives a reason. Returns what was done, or `None` if there is nothing to set
/// back.
pub async fn after_early_stop(opt: &Options) -> Option<String> {
    let text = tokio::fs::read_to_string(&*FILE).await.ok()?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let result = async {
        let oracle = Oracle::connect(&opt.address).await?;
        if let Some(why) = source_refused(&oracle, opt, &lines).await {
            oracle.close().await;
            return Ok(Err(why));
        }
        let missed = write(oracle.avr(), QUERIES, &lines).await;
        oracle.close().await;
        missed.map(Ok)
    }
    .await;
    Some(match result {
        Ok(Ok(missed)) if missed.is_empty() => {
            forget().await;
            format!("{} settings of the last run set back", lines.len())
        }
        // The file stays: the next run tries again.
        Ok(Ok(missed)) => format!(
            "settings of the last run set back, not taken: {missed:?}. They stay in {}",
            FILE.display()
        ),
        Ok(Err(why)) => format!(
            "settings of the last run not set back, because the source cannot be set back: {why}. They stay in {}",
            FILE.display()
        ),
        Err(e) => format!(
            "settings of the last run not set back: {}. They are in {}",
            e.msg,
            FILE.display()
        ),
    })
}

/// Why the saved source is not the source now and cannot be set back. `None` if the lines have no
/// source, if it is the source now, or if it can be set back. A marked source (see
/// [`mark_source`]) is never set back. Some settings are kept for each source, so without the
/// saved source no setting is sent.
async fn source_refused(oracle: &Oracle, opt: &Options, lines: &[String]) -> Option<String> {
    if !lines
        .iter()
        .any(|l| l.starts_with(SOURCE) || l.starts_with(MARKED_SOURCE))
    {
        return None;
    }
    let state = oracle.state();
    let zone = state.zone(Zone::Main);
    if lines.iter().any(|l| l.starts_with(MARKED_SOURCE)) {
        return match single_code(lines, MARKED_SOURCE) {
            Some(code) if zone.source.as_deref() == Some(code) => None,
            Some(code) => Some(format!(
                "the start source {code} is not selected, and this file does not set it back"
            )),
            None => Some("more than one saved source".to_owned()),
        };
    }
    match saved_source(lines) {
        Some(code) if zone.source.as_deref() == Some(code) => None,
        None => Some("more than one saved source".to_owned()),
        // A source command can switch the zone on, at a volume that nothing lowers here.
        Some(_) if zone.power != Some(denon_marantz::proto::Power::On) => {
            Some("the main zone is not on".to_owned())
        }
        Some(code) => checks::source_set_back(oracle, opt, code).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_kept_in_the_order_of_the_queries() {
        let mut lines = vec!["PSBAS 50", "MSSTEREO", "PSTONE CTRL ON", "CVFL 50"];
        lines.sort_by_key(|l| position(l));
        assert_eq!(
            vec!["MSSTEREO", "PSTONE CTRL ON", "PSBAS 50", "CVFL 50"],
            lines
        );
    }

    #[test]
    fn only_one_saved_source_is_used() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(Some("TV"), saved_source(&lines(&["SITV", "MSSTEREO"])));
        assert_eq!(None, saved_source(&lines(&["MSSTEREO"])));
        assert_eq!(
            None,
            saved_source(&lines(&["SITV", "SIBD"])),
            "changed during the read"
        );
        let mut marked = lines(&["SIBD", "MSSTEREO"]);
        mark_source(&mut marked);
        assert_eq!(lines(&["#SIBD", "MSSTEREO"]), marked);
        assert_eq!(None, saved_source(&marked), "never sent");
        assert_eq!(Some("BD"), single_code(&marked, MARKED_SOURCE));
        assert!(ensure_source(&lines(&["SITV"])).is_ok());
        assert!(ensure_source(&marked).is_err(), "marked");
        assert!(ensure_source(&lines(&["SITV", "SIBD"])).is_err());
        let two = lines(&["SITV", "SIBD", "MSSTEREO"]);
        assert!(!taken(&two, "SITV"), "changed during the read");
        assert!(taken(&two, "MSSTEREO"));
        assert!(taken(&lines(&["SITV"]), "SITV"));
    }

    #[test]
    fn lines_of_the_main_zone_and_all_zones_are_read() {
        assert!(for_main_zone(EventZone::Main));
        assert!(for_main_zone(EventZone::All));
        assert!(!for_main_zone(EventZone::Zone2));
        assert!(!for_main_zone(EventZone::Zone3));
    }

    #[test]
    fn list_ends_and_quick_selects_are_not_saved() {
        assert!(!is_saved("CVEND"));
        assert!(!is_saved("MSQUICK1"));
        assert!(!is_saved("MVMAX 98"));
        assert!(is_saved("VSAUDIO TV"));
        assert!(is_saved("BTTX SP"));
    }

    #[test]
    fn a_line_answers_the_longest_query_it_starts_with() {
        let query = |line| position(line).map(|i| QUERIES[i]);
        assert_eq!(Some("SSTTRLEV ?"), query("SSTTRLEV 50"));
        assert_eq!(Some("SSTTR ?"), query("SSTTR OFF"));
        assert_eq!(Some("PSDELAY ?"), query("PSDELAY 000"));
        assert_eq!(Some("PSDEL ?"), query("PSDEL 000"));
        assert_eq!(Some("PSIMAX ?"), query("PSIMAX OFF"));
        assert_eq!(Some("PSIMAXAUD ?"), query("PSIMAXAUD AUTO"));
        assert_eq!(Some("PSIMAXHPF ?"), query("PSIMAXHPF 080"));
        assert_eq!(Some("PSIMAXLPF ?"), query("PSIMAXLPF 120"));
        // The tactile channel is also in the `CV?` list, and goes back before the transducer.
        assert_eq!(Some("CVTTR ?"), query("CVTTR 50"));
        assert!(position("CVTTR ON").unwrap() < position("SSTTR OFF").unwrap());
        assert_eq!(Some("SPPR ?"), query("SPPR 2"));
        assert_eq!(Some("PSAUROPR ?"), query("PSAUROPR SMA"));
        assert!(position("SPPR 2").unwrap() < position("PSDIRAC 1").unwrap());
        assert_eq!(Some("SI?"), query("SISAT/CBL"));
        assert!(position("SIMPLAY").unwrap() < position("MSSTEREO").unwrap());
        assert_eq!(None, query("PWON"));
    }

    #[test]
    fn only_lines_that_differ_are_sent() {
        let lines = [
            "MSSTEREO",
            "PSDIRAC 2",
            "PSTONE CTRL OFF",
            "PSBAS 50",
            "CVFL 50",
        ]
        .map(String::from);
        let now = [
            "MSSTEREO",
            "PSDIRAC 2",
            "PSTONE CTRL OFF",
            "PSBAS 50",
            "CVFL 48",
        ]
        .map(String::from);
        assert_eq!(
            vec!["CVFL 50"],
            with_detour(&lines, differing(&lines, &now))
        );
        // A different bass needs the Dirac and tone control lines.
        let now = [
            "MSSTEREO",
            "PSDIRAC 2",
            "PSTONE CTRL OFF",
            "PSBAS 49",
            "CVFL 50",
        ]
        .map(String::from);
        assert_eq!(
            vec!["PSDIRAC 2", "PSTONE CTRL OFF", "PSBAS 50"],
            with_detour(&lines, differing(&lines, &now))
        );
        assert!(differing(&lines, &lines).is_empty());
    }

    #[test]
    fn tone_control_is_on_while_bass_and_treble_are_sent() {
        let lines = ["PSTONE CTRL OFF", "PSBAS 50", "PSTRE 48", "CVFL 50"].map(String::from);
        assert_eq!(
            vec![
                "PSTONE CTRL ON",
                "PSBAS 50",
                "PSTRE 48",
                "PSTONE CTRL OFF",
                "CVFL 50"
            ],
            send_order(&lines)
        );
        let lines = ["PSDIRAC 2", "PSMULTEQ:OFF", "PSTONE CTRL OFF", "PSBAS 50"].map(String::from);
        assert_eq!(
            vec![
                "PSDIRAC OFF",
                "PSMULTEQ:OFF",
                "PSTONE CTRL ON",
                "PSBAS 50",
                "PSTONE CTRL OFF",
                "PSDIRAC 2"
            ],
            send_order(&lines)
        );
        // A saved value that allows bass and treble is sent once.
        let lines = ["PSDIRAC OFF", "PSTONE CTRL ON", "PSBAS 50"].map(String::from);
        assert_eq!(
            vec!["PSDIRAC OFF", "PSTONE CTRL ON", "PSBAS 50"],
            send_order(&lines)
        );
        // Without bass or treble the order stays.
        let lines = ["PSDIRAC 2", "PSTONE CTRL OFF", "CVFL 50"].map(String::from);
        assert_eq!(
            vec!["PSDIRAC 2", "PSTONE CTRL OFF", "CVFL 50"],
            send_order(&lines)
        );
    }
}
