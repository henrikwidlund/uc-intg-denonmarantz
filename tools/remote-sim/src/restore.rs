// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Saves the receiver settings that the `simple_commands` check changes, and the source (see
//! [`mark_source`]), and sets them back. The saved lines are also written to a file. If a run stops
//! before the settings are set back, the next run sets them back at its start.

use crate::oracle::Oracle;
use crate::{Fail, Options, checks};
use denon_marantz::proto::source::telnet_to_display_source;
use denon_marantz::proto::telnet::EventZone;
use denon_marantz::proto::{Power, Volume};
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
/// this order for bass and treble, and [`write()`] sends a source and a speaker preset that differ
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
/// [`QUERIES`]. Each answer is also the command that sets the value again. A skip if a value
/// changed during the read, see [`changed_values`]: then it is not known which line is the value.
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
        let mut line = format!("{}{}", event.event.code(), event.parameter);
        // Like the library for the source code: spaces at the end are not part of a value.
        line.truncate(line.trim_end().len());
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
    let changed = changed_values(&lines);
    if !changed.is_empty() {
        let pairs: Vec<String> = changed
            .iter()
            .map(|(first, then)| format!("{first:?} then {then:?}"))
            .collect();
        return Err(Fail::skip(format!(
            "a value changed while the receiver was read: {}",
            pairs.join(", ")
        )));
    }
    Ok(lines)
}

/// Pairs of lines of one kind (see [`answer_kind`]) with different values: the value changed
/// during the read. The first line of the kind comes first. Source lines are left out:
/// [`saved_source`] and [`taken`] do not use two source codes.
fn changed_values(lines: &[String]) -> Vec<(&str, &str)> {
    let mut pairs = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with(SOURCE) {
            continue;
        }
        let Some(kind) = answer_kind(line) else {
            continue;
        };
        if let Some(first) = lines[..i]
            .iter()
            .find(|l| l != &line && answer_kind(l) == Some(kind))
        {
            pairs.push((first.as_str(), line.as_str()));
        }
    }
    pairs
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

/// Lines that set a value. `CVEND` ends the list of channel levels and `SSTTR END` the transducer
/// lines. `MSQUICK` is a quick select memory and `MSSMART` a Smart Select memory: they are not
/// settings, and a memory recalls the source and the volume. A source line without a code sets
/// nothing.
fn is_saved(line: &str) -> bool {
    position(line).is_some()
        && line != "CVEND"
        && line != "SSTTR END"
        && !line.starts_with("MSQUICK")
        && !line.starts_with("MSSMART")
        && line
            .strip_prefix(SOURCE)
            .is_none_or(|code| source_code(code).is_some())
}

/// Send the saved lines that differ from the receiver, read with `queries`. Lines that did not come
/// back are sent once more. A source and a speaker preset that differ are sent first, each alone,
/// and the receiver is read again after each. Nothing is sent, or nothing more, if the source
/// differs and `source` is [`SourceSend::Never`] or the main zone is not on, if the receiver did
/// not take the source or the speaker preset, if it does not report the source later (also in
/// the last read after the retry), or if the marked lines (see [`mark_source`]) do not hold
/// exactly one code or another source than that one is selected in any read. The last read after
/// the retry always asks for the source when the lines hold one. Nothing is sent if the lines hold
/// a marked and an unmarked source line with a code, see [`mixed_sources`]. Then all lines without
/// the marked ones are returned as [`Missed::NotSent`], with the reason. Else returns as
/// [`Missed::Lines`] the lines that the receiver did not take: they have a different value when
/// read again. Every read asks for the source when the lines hold one: `SI?` is added to `queries`
/// if it is missing.
pub async fn write(
    avr: &Avr,
    queries: &[&str],
    lines: &[String],
    source: SourceSend,
) -> Result<Missed, Fail> {
    if mixed_sources(lines) {
        let unmarked = lines.iter().filter(|l| !l.starts_with(MARKED_SOURCE));
        return Ok(Missed::NotSent(unmarked.cloned().collect(), MIXED_SOURCES));
    }
    let marked = lines.iter().any(|l| l.starts_with(MARKED_SOURCE));
    let start = single_code(lines, MARKED_SOURCE);
    let lines: Vec<String> = lines
        .iter()
        .filter(|l| !l.starts_with(MARKED_SOURCE))
        .cloned()
        .collect();
    let lines = lines.as_slice();
    let has_source = marked
        || lines
            .iter()
            .any(|l| l.strip_prefix(SOURCE).and_then(source_code).is_some());
    let queries = with_source_query(has_source, queries);
    let queries = queries.as_ref();
    // Only the lines that differ are sent, so nothing changes that does not need to.
    let mut now = read(avr, queries).await?;
    // The marked source is not set back, so nothing is sent while another source is selected.
    if not_start_source(marked, start, &now) {
        return Ok(Missed::NotSent(lines.to_vec(), NOT_START_SOURCE));
    }
    // Some settings are kept for each source, and a speaker preset changes the settings of the
    // preset. So both go first and alone, and the other lines are compared with the values of the
    // saved source and preset.
    for prefix in [SOURCE, PRESET] {
        if let Some(line) = lines
            .iter()
            .find(|line| line.starts_with(prefix) && !taken(&now, line))
        {
            if prefix == SOURCE {
                // The caller checked the source rule only for a source line that may be sent.
                if source == SourceSend::Never {
                    return Ok(Missed::NotSent(lines.to_vec(), SOURCE_NOT_ALLOWED));
                }
                // A source command can switch the zone on, at a volume that nothing lowers here.
                if avr.snapshot().zone(Zone::Main).power != Some(Power::On) {
                    return Ok(Missed::NotSent(lines.to_vec(), ZONE_NOT_ON));
                }
            }
            send(avr, std::slice::from_ref(line)).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            now = read(avr, queries).await?;
            if not_start_source(marked, start, &now) {
                return Ok(Missed::NotSent(lines.to_vec(), NOT_START_SOURCE));
            }
            // Without the saved source or preset, the other lines would change the settings of
            // another source or preset.
            if !taken(&now, line) {
                let why = if prefix == SOURCE {
                    SOURCE_NOT_TAKEN
                } else {
                    PRESET_NOT_TAKEN
                };
                return Ok(Missed::NotSent(lines.to_vec(), why));
            }
        }
    }
    let changed = with_detour(lines, differing(lines, &now));
    if changed.is_empty() {
        return Ok(Missed::Lines(changed));
    }
    // A source line is sent only alone, after the power check above: here the receiver lost the
    // source again, or did not report it.
    if changed.iter().any(|l| l.starts_with(SOURCE)) {
        return Ok(Missed::NotSent(lines.to_vec(), SOURCE_LOST));
    }
    send(avr, &changed).await?;
    // The receiver needs time to apply a sound mode or a speaker preset.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let now = read(avr, queries).await?;
    if not_start_source(marked, start, &now) {
        return Ok(Missed::NotSent(lines.to_vec(), NOT_START_SOURCE));
    }
    let missed = differing(lines, &now);
    if missed.is_empty() {
        return Ok(Missed::Lines(missed));
    }
    // A busy receiver can answer too late, or ignore a line while it applies the source, the sound
    // mode or a speaker preset. Send the missed lines again and ask for them and the values they
    // can change, see [`retry_queries`]. The Dirac and tone control lines are checked again too,
    // because the retry changes them. A Dirac change can change MultEQ and Dynamic EQ, and a preset
    // change can change all settings. A source line is sent only alone, after the power check
    // above.
    if missed.iter().any(|l| l.starts_with(SOURCE)) {
        return Ok(Missed::NotSent(lines.to_vec(), SOURCE_LOST));
    }
    let missed = with_detour(lines, missed);
    send(avr, &missed).await?;
    let slow = missed
        .iter()
        .any(|l| l.starts_with(SOUND_MODE) || l.starts_with(PRESET));
    tokio::time::sleep(Duration::from_secs(if slow { 2 } else { 1 })).await;
    let again = retry_queries(&missed, queries, has_source);
    let now = read(avr, &again).await?;
    if not_start_source(marked, start, &now) {
        return Ok(Missed::NotSent(lines.to_vec(), NOT_START_SOURCE));
    }
    Ok(source_lost(
        lines,
        lines
            .iter()
            .filter(|line| position(line).is_some_and(|i| again.contains(&QUERIES[i])))
            .filter(|line| !taken(&now, line))
            .cloned()
            .collect(),
    ))
}

/// Another source than the start source `start` of the marked lines is selected in the read `now`,
/// or `now` or the marked lines do not hold exactly one source code. False without marked lines.
fn not_start_source(marked: bool, start: Option<&str>, now: &[String]) -> bool {
    marked && !start.is_some_and(|code| single_code(now, SOURCE) == Some(code))
}

/// `queries` with `SI?` at the end if `source` and it is missing: a read must show the source when
/// the lines hold one.
fn with_source_query<'a, 'q>(
    source: bool,
    queries: &'a [&'q str],
) -> std::borrow::Cow<'a, [&'q str]> {
    if source && !queries.contains(&"SI?") {
        std::borrow::Cow::Owned([queries, &["SI?"]].concat())
    } else {
        std::borrow::Cow::Borrowed(queries)
    }
}

/// The queries of the last read of [`write()`], after the retry of the lines `missed`, each once.
/// The Dirac dependent values are asked after a Dirac line, all `queries` after a speaker preset or
/// a sound mode line. With `source`, the source is always asked: the read must show a lost source.
fn retry_queries<'q>(missed: &[String], queries: &[&'q str], source: bool) -> Vec<&'q str> {
    let mut retry: Vec<&'q str> = missed
        .iter()
        .filter_map(|line| position(line))
        .map(|i| QUERIES[i])
        .collect();
    if missed.iter().any(|line| line.starts_with(DIRAC)) {
        retry.extend(DIRAC_DEPENDENT);
    }
    // A preset or a sound mode change can change the other settings.
    if missed
        .iter()
        .any(|line| line.starts_with(PRESET) || line.starts_with(SOUND_MODE))
    {
        retry.extend(queries);
    }
    if source {
        retry.push("SI?");
    }
    let mut again: Vec<&'q str> = Vec::with_capacity(retry.len());
    for query in retry {
        if !again.contains(&query) {
            again.push(query);
        }
    }
    again
}

/// The lines `missed` of the last read of [`write()`] as [`Missed::Lines`]. If they hold a source
/// line, the receiver lost the source in the retry: all `lines` are [`Missed::NotSent`] with
/// [`SOURCE_LOST`], as in the earlier steps of [`write()`].
fn source_lost(lines: &[String], missed: Vec<String>) -> Missed {
    if missed.iter().any(|l| l.starts_with(SOURCE)) {
        Missed::NotSent(lines.to_vec(), SOURCE_LOST)
    } else {
        Missed::Lines(missed)
    }
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

/// If [`write()`] may send a source line that differs from the receiver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceSend {
    /// The caller checked that the saved source and the current source may be selected. [`write()`]
    /// does not check the source of its own first read against the source that the caller checked.
    Allowed,
    /// A source line that differs stops the write: [`Missed::NotSent`].
    Never,
}

/// The saved lines that are not set back, see [`write()`].
#[derive(Debug, PartialEq)]
pub enum Missed {
    /// Lines that were sent, but the receiver did not take.
    Lines(Vec<String>),
    /// [`write()`] stopped early because of the source or the speaker preset, for the reason in the
    /// text. Some of these lines may already be sent, but they were not checked.
    NotSent(Vec<String>, &'static str),
}

/// Reasons of [`Missed::NotSent`].
const NOT_START_SOURCE: &str =
    "the start source is not selected, or the receiver did not report a single source";
const SOURCE_NOT_ALLOWED: &str = "the receiver source is not the saved source or is not known, and \
     this set back may not select a source";
const ZONE_NOT_ON: &str = "the source differs or is not known, and the main zone is not on";
const SOURCE_NOT_TAKEN: &str = "the receiver did not take the saved source";
const PRESET_NOT_TAKEN: &str = "the receiver did not take the saved speaker preset";
const SOURCE_LOST: &str = "the receiver lost the saved source again, or did not report it";
const MIXED_SOURCES: &str = "the lines have a marked and an unmarked source line with a code";

impl Missed {
    /// Take the sound mode lines out of lines that the receiver did not take. The receiver refuses
    /// a sound mode that does not fit the input signal, so such a line is only a note: it does not
    /// keep [`FILE`]. Lines that were not sent are kept.
    pub fn take_sound_mode(&mut self) -> Vec<String> {
        match self {
            Self::Lines(lines) => lines
                .extract_if(.., |line| line.starts_with(SOUND_MODE))
                .collect(),
            Self::NotSent(..) => Vec::new(),
        }
    }

    /// All lines, sent or not.
    pub fn into_lines(self) -> Vec<String> {
        match self {
            Self::Lines(lines) | Self::NotSent(lines, _) => lines,
        }
    }
}

/// Text after a sound mode that the receiver did not take.
pub const SOUND_NOTE: &str = " (the input signal can limit the sound modes)";

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

/// Mark the source lines: [`write()`] never sends them, but sends no other line while another
/// source is selected. Some settings are kept for each source.
pub fn mark_source(lines: &mut [String]) {
    for line in lines.iter_mut().filter(|l| l.starts_with(SOURCE)) {
        line.insert(0, '#');
    }
}

/// The receiver has `line`. A source line counts only if the receiver gave exactly one source: with
/// two, the source changed during the read. A source line without a code sets nothing, so it
/// counts as taken.
fn taken(now: &[String], line: &str) -> bool {
    match line.strip_prefix(SOURCE) {
        Some(code) => source_code(code).is_none_or(|code| single_code(now, SOURCE) == Some(code)),
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

/// The source of the saved lines. `None` if no line has a source code, or more than one does: the
/// source changed during the read, and none of them is known to be the start source.
pub fn saved_source(lines: &[String]) -> Option<&str> {
    single_code(lines, SOURCE)
}

/// The source code after `SI`, without spaces at the end, like the library reads it. `None` for a
/// blank code.
fn source_code(code: &str) -> Option<&str> {
    Some(code.trim_end()).filter(|code| !code.is_empty())
}

/// The source code of the only line that starts with `prefix`. `None` if there is no such line, or
/// more than one. Lines with a blank code are left out.
fn single_code<'a>(lines: &'a [String], prefix: &str) -> Option<&'a str> {
    let mut codes = lines
        .iter()
        .filter_map(|l| l.strip_prefix(prefix).and_then(source_code));
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
/// [`file_source`] gives a reason. The volume is read before the write, and after the write a
/// volume above it or above the limit is lowered (see [`lower_to`]). If that read fails, only a
/// volume above the limit is lowered, and the result says so. The write itself is not watched: a
/// rise is lowered only after it. Returns what was done, with the lines of the file that [`load`]
/// leaves out, or `None` if there is nothing to set back.
pub async fn after_early_stop(opt: &Options) -> Option<String> {
    let text = tokio::fs::read_to_string(&*FILE).await.ok()?;
    let (lines, left_out) = load(&text);
    let result = async {
        let oracle = Oracle::connect(&opt.address).await?;
        let source = match file_source(&oracle, opt, &lines).await {
            Ok(source) => source,
            Err(why) => {
                oracle.close().await;
                return Ok(Err(why));
            }
        };
        // The write or the owner can change the volume, for example with a speaker preset.
        let start = oracle.fresh_volume().await;
        let missed = write(oracle.avr(), QUERIES, &lines, source).await;
        let limit = start
            .as_ref()
            .map_or(checks::MAX_VOLUME, |s| s.min(checks::MAX_VOLUME));
        let mut volume = lower_to(&oracle, limit).await;
        if let Err(e) = start {
            volume += &format!(". The volume before the set back is not known: {e}");
        }
        oracle.close().await;
        match missed {
            Ok(missed) => Ok(Ok((missed, volume))),
            Err(e) => Err(Fail::new(e.msg + &volume)),
        }
    }
    .await;
    let note = match result {
        Ok(Ok((Missed::NotSent(_, why), volume))) => format!(
            "settings of the last run not set back, the set back stopped: {why}. They stay in {}{volume}",
            FILE.display()
        ),
        Ok(Ok((mut missed, volume))) => {
            let sound = missed.take_sound_mode();
            let mut missed = missed.into_lines();
            if !missed.is_empty() {
                // The file stays: the next run tries again.
                missed.extend(sound);
                missed.sort_by_key(|line| position(line));
                format!(
                    "settings of the last run set back, not taken: {missed:?}. They stay in {}{volume}",
                    FILE.display()
                )
            } else {
                forget().await;
                let set = sent_count(&lines).saturating_sub(sound.len());
                if sound.is_empty() {
                    format!("{set} settings of the last run set back{volume}")
                } else {
                    format!(
                        "{set} settings of the last run set back, sound mode not taken: {sound:?}{SOUND_NOTE}{volume}"
                    )
                }
            }
        }
        Ok(Err(why)) => format!(
            "settings of the last run not set back: {why}. They stay in {}",
            FILE.display()
        ),
        Err(e) => format!(
            "settings of the last run not set back: {}. They stay in {}",
            e.msg,
            FILE.display()
        ),
    };
    Some(if left_out.is_empty() {
        note
    } else {
        format!("{note}. Lines of the file that are never sent: {left_out:?}")
    })
}

/// The number of `lines` that [`write()`] can send: a marked source line and a source line without
/// a code are never sent.
pub fn sent_count(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|l| {
            !l.starts_with(MARKED_SOURCE)
                && l.strip_prefix(SOURCE)
                    .is_none_or(|c| source_code(c).is_some())
        })
        .count()
}

// `lower_to` sends the limit with `Volume::from_db_rounded`: a limit with a whole or a half dB
// value is sent exactly. A volume that the receiver reports is in half dB steps.
const _: () = assert!((checks::MAX_VOLUME * 2.0) as i64 as f64 == checks::MAX_VOLUME * 2.0);

/// Lower the volume of the main zone to [`checks::MAX_VOLUME`] if the receiver has a higher one,
/// see [`lower_to`].
pub async fn cap_volume(oracle: &Oracle) -> String {
    lower_to(oracle, checks::MAX_VOLUME).await
}

/// Lower the volume of the main zone to `limit` if the receiver has a higher one. Nothing is sent
/// if the volume is not above `limit`. The volume is read just before the send: a volume that
/// someone lowers in that short time can be set to `limit`. Returns a note that starts with `. `,
/// or an empty text only if the volume is known and not above the limit.
async fn lower_to(oracle: &Oracle, limit: f64) -> String {
    let volume = match oracle.fresh_volume().await {
        Ok(v) if v <= limit => return String::new(),
        Ok(v) => v,
        Err(e) => return format!(". The volume is not known: {}", e.msg),
    };
    // The scale of the Remote is dB plus 80.
    let Some(target) = Volume::from_db_rounded((limit - 80.0) as f32) else {
        return format!(". The volume {volume} is above the limit {limit}, not lowered");
    };
    if let Err(e) = oracle
        .avr()
        .telnet_send(Zone::Main, Command::SetVolume(target))
        .await
    {
        return format!(". The volume {volume} is above the limit {limit}, not lowered: {e}");
    }
    match oracle.fresh_volume().await {
        Ok(after) if after <= limit => {
            format!(". The volume {volume} was above the limit {limit}, it was lowered to {after}")
        }
        Ok(after) => format!(
            ". The volume {volume} is above the limit {limit}, it is {after} after the lowering"
        ),
        Err(e) => format!(
            ". The volume {volume} was above the limit {limit}, it was lowered, but the volume after it is not known: {}",
            e.msg
        ),
    }
}

/// The lines of the text of [`FILE`] that [`write()`] may get, and the lines that are left out. A
/// line is kept if [`is_saved`] takes it, or if it is a source line. `#` marks only a source line.
/// Like [`read`], spaces at the end of a line are removed. A source line with a blank code is kept,
/// so that a file with only a blank source line is refused. All source lines are kept, so that a
/// file with two source codes is refused. Of the other lines, only the first answer of each kind
/// (see [`answer_kind`]) is kept. A line that is left out counts as the answer of its kind too if
/// [`is_saved`] takes it without a `#` before it, so a later line of that kind is left out. A line
/// that sets no value, such as `MSQUICK1`, does not count. An All Zone Stereo line other than
/// `MNZST OFF` is left out, see [`all_zone_stereo_ok`], and so is a sleep timer that counts down,
/// see [`sleep_counts`]. Blank lines are not returned. The kept lines are sorted as [`read`] sorts
/// them, by the place of their query, with the marked source lines first. The sort is stable.
fn load(text: &str) -> (Vec<String>, Vec<String>) {
    let mut kept: Vec<String> = Vec::new();
    let mut left_out = Vec::new();
    // The kinds of the value lines before, kept or left out.
    let mut seen: Vec<(usize, &str)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let after_mark = line.strip_prefix('#');
        let unmarked = after_mark.unwrap_or(line);
        if unmarked.starts_with(SOURCE) {
            kept.push(line.to_owned());
            continue;
        }
        let kind = answer_kind(unmarked);
        let first = match kind {
            Some(_) if !is_saved(unmarked) => true,
            Some(kind) if seen.contains(&kind) => false,
            Some(kind) => {
                seen.push(kind);
                true
            }
            None => true,
        };
        if first
            && after_mark.is_none()
            && is_saved(line)
            && all_zone_stereo_ok(line)
            && !sleep_counts(line)
        {
            kept.push(line.to_owned());
        } else {
            left_out.push(line.to_owned());
        }
    }
    kept.sort_by_key(|line| (!line.starts_with(MARKED_SOURCE), position(line)));
    (kept, left_out)
}

/// The line is a sleep timer that counts down. Its value goes down each minute, so an old value
/// would set a longer time: it is not saved and not sent.
pub fn sleep_counts(line: &str) -> bool {
    line.starts_with("SLP") && line != "SLPOFF"
}

/// Remove a sleep timer that counts down from the saved `lines`, see [`sleep_counts`].
pub fn leave_out_counting_sleep(lines: &mut Vec<String>) {
    lines.retain(|line| !sleep_counts(line));
}

/// The line is not an All Zone Stereo line, or it is exactly `MNZST OFF`. All Zone Stereo can
/// switch on other zones, so no other spelling is sent.
fn all_zone_stereo_ok(line: &str) -> bool {
    !line.starts_with("MNZST") || line == "MNZST OFF"
}

/// Queries whose answer has more than one line, one line for each channel. Each channel is a kind
/// of its own, see [`answer_kind`].
const MULTI_LINE: &[&str] = &["CV?"];

/// Queries that the receiver answers with two lines: on or off, and a value. Each of them is a
/// separate answer. `BTTX ?` gives the output mode as value, `CVTTR ?` the level.
const ON_OFF_AND_VALUE: &[&str] = &["BTTX ?", "CVTTR ?"];

/// The kind of answer of `line`: the query as an index in [`QUERIES`], and a part that tells apart
/// the lines of one query. For a query in [`MULTI_LINE`] the part is the channel, the text before
/// the space, for example `CVFL`. For a query in [`ON_OFF_AND_VALUE`] the on or off line has the
/// part `ON/OFF`. For all other lines the part is empty.
fn answer_kind(line: &str) -> Option<(usize, &str)> {
    position(line).map(|i| {
        let query = QUERIES[i];
        let part = if MULTI_LINE.contains(&query) {
            line.split_once(' ').map_or(line, |(channel, _)| channel)
        } else if ON_OFF_AND_VALUE.contains(&query)
            && line
                .strip_prefix(query.trim_end_matches('?').trim_end())
                .is_some_and(|rest| matches!(rest.trim_end(), " ON" | " OFF"))
        {
            "ON/OFF"
        } else {
            ""
        };
        (i, part)
    })
}

/// The receiver source `now` is the saved source `code`. The library keeps some network sources
/// by their display name, for example `Internet Radio` for `IRADIO`.
pub fn is_source_now(now: Option<&str>, code: &str) -> bool {
    now.is_some_and(|now| now == code || now == telnet_to_display_source(code))
}

/// What [`write()`] may do with the source of the file `lines`, or why nothing is sent. The source
/// lines must pass [`source_lines_refused`]. A marked source (see [`mark_source`]) is never set
/// back. Some settings are kept for each source, so without the saved source no setting is sent.
/// Nothing is sent while the main zone is not on: a receiver in standby answers few queries.
/// [`SourceSend::Allowed`] only if the saved source is not the source now and may be set back. If
/// it is the source now, [`SourceSend::Never`]: the source rule is not checked, so [`write()`]
/// sends no source line if the source changes before it.
async fn file_source(
    oracle: &Oracle,
    opt: &Options,
    lines: &[String],
) -> Result<SourceSend, String> {
    if let Some(why) = source_lines_refused(lines) {
        return Err(why.to_owned());
    }
    let state = oracle.state();
    let zone = state.zone(Zone::Main);
    if zone.power != Some(Power::On) {
        return Err("the main zone is not on".to_owned());
    }
    if lines.iter().any(|l| l.starts_with(MARKED_SOURCE)) {
        return match single_code(lines, MARKED_SOURCE) {
            Some(code) if is_source_now(zone.source.as_deref(), code) => Ok(SourceSend::Never),
            Some(code) => Err(format!(
                "the start source {code} is not selected, and this file does not set it back"
            )),
            None => Err("no single saved source".to_owned()),
        };
    }
    match saved_source(lines) {
        Some(code) if is_source_now(zone.source.as_deref(), code) => Ok(SourceSend::Never),
        None => Err("no single saved source".to_owned()),
        Some(code) => match checks::source_set_back(oracle, opt, code).await {
            Some(why) => Err(why),
            None => Ok(SourceSend::Allowed),
        },
    }
}

/// Why the source lines of the file `lines` are refused, or `None`. A file without a source line
/// is refused: the settings could go to another source. A file with a marked source line and an
/// unmarked source line with a code, see [`mixed_sources`], is refused too.
fn source_lines_refused(lines: &[String]) -> Option<&'static str> {
    if !lines
        .iter()
        .any(|l| l.starts_with(SOURCE) || l.starts_with(MARKED_SOURCE))
    {
        return Some("the file has no source line, so the settings could go to another source");
    }
    mixed_sources(lines).then_some(
        "the file has a marked and an unmarked source line, and the unmarked one would be sent",
    )
}

/// The lines have a marked source line and also an unmarked source line with a code. [`write()`]
/// would send the unmarked line and select another source than the marked one.
fn mixed_sources(lines: &[String]) -> bool {
    lines.iter().any(|l| l.starts_with(MARKED_SOURCE))
        && lines
            .iter()
            .any(|l| l.strip_prefix(SOURCE).and_then(source_code).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn a_read_without_the_marked_start_source_stops_the_write() {
        let start = Some("CD");
        assert!(!not_start_source(false, None, &owned(&["SITV"])));
        assert!(!not_start_source(
            true,
            start,
            &owned(&["SICD", "MSSTEREO"])
        ));
        assert!(not_start_source(true, start, &owned(&["SITV"])));
        assert!(not_start_source(true, start, &owned(&["MSSTEREO"])));
        assert!(not_start_source(true, start, &owned(&["SICD", "SITV"])));
        assert!(not_start_source(true, None, &owned(&["SICD"])));
    }

    #[test]
    fn the_last_read_asks_for_the_source_when_the_lines_hold_one() {
        let missed = owned(&["PSRSTR OFF", "PSDIRAC OFF"]);
        assert_eq!(
            retry_queries(&missed, QUERIES, false),
            ["PSRSTR ?", "PSDIRAC ?", "PSMULTEQ: ?", "PSDYNEQ ?"]
        );
        assert_eq!(
            retry_queries(&missed, QUERIES, true),
            ["PSRSTR ?", "PSDIRAC ?", "PSMULTEQ: ?", "PSDYNEQ ?", "SI?"]
        );
        let preset = owned(&["SPPR 1"]);
        let again = retry_queries(&preset, &["SI?", "SPPR ?"], true);
        let sound = ["MSSTEREO".to_owned()];
        assert_eq!(
            vec!["MS?", "SI?", "SPPR ?"],
            retry_queries(&sound, &["SI?", "SPPR ?"], false),
            "a sound mode can change the other settings"
        );
        assert_eq!(&["PW?", "SI?"][..], &*with_source_query(true, &["PW?"]));
        assert_eq!(
            &["SI?", "PW?"][..],
            &*with_source_query(true, &["SI?", "PW?"])
        );
        assert_eq!(&["PW?"][..], &*with_source_query(false, &["PW?"]));
        assert_eq!(again, ["SPPR ?", "SI?"]);
    }

    #[test]
    fn sound_mode_lines_are_taken_out_only_after_a_send() {
        let lines = || vec!["MSDOLBY AUDIO-DSUR".to_owned(), "PSRSTR OFF".to_owned()];
        let mut missed = Missed::Lines(lines());
        assert_eq!(missed.take_sound_mode(), ["MSDOLBY AUDIO-DSUR"]);
        assert!(missed.take_sound_mode().is_empty());
        assert_eq!(missed.into_lines(), ["PSRSTR OFF"]);
        let mut not_sent = Missed::NotSent(lines(), SOURCE_LOST);
        assert!(not_sent.take_sound_mode().is_empty());
        assert_eq!(not_sent.into_lines(), lines());
    }

    #[test]
    fn a_source_line_in_the_last_read_stops_the_write() {
        let lines = vec!["SITV".to_owned(), "PSRSTR OFF".to_owned()];
        let missed = vec!["PSRSTR OFF".to_owned()];
        assert_eq!(source_lost(&lines, missed.clone()), Missed::Lines(missed));
        assert_eq!(source_lost(&lines, Vec::new()), Missed::Lines(Vec::new()));
        assert_eq!(
            source_lost(&lines, vec!["SITV".to_owned()]),
            Missed::NotSent(lines.clone(), SOURCE_LOST)
        );
    }

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
    fn source_codes_are_trimmed_and_blank_codes_are_ignored() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(Some("TV"), saved_source(&lines(&["SITV  ", "MSSTEREO"])));
        assert_eq!(Some("TV"), saved_source(&lines(&["SITV", "SI", "SI  "])));
        assert_eq!(None, saved_source(&lines(&["SI", "MSSTEREO"])));
        assert_eq!(None, saved_source(&lines(&["SI "])));
        assert_eq!(
            Some("BD"),
            single_code(&lines(&["#SIBD ", "#SI"]), MARKED_SOURCE)
        );
        assert!(taken(&lines(&["SITV "]), "SITV"));
        assert!(taken(&lines(&["SITV"]), "SITV "));
        assert!(taken(&lines(&["SITV", "SI"]), "SITV"), "blank code");
        assert!(taken(&lines(&["SIBD"]), "SI "), "sets nothing");
        assert!(!taken(&lines(&["SIBD"]), "SITV "));
        assert!(!is_saved("SI"));
        assert!(!is_saved("SI  "));
        assert!(is_saved("SITV"));
    }

    #[test]
    fn a_marked_and_an_unmarked_source_are_mixed() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(mixed_sources(&lines(&["#SIBD", "SITV", "MSSTEREO"])));
        assert!(mixed_sources(&lines(&["SITV", "#SI"])), "blank marked code");
        assert!(!mixed_sources(&lines(&["#SIBD", "SI"])), "blank code");
        assert!(!mixed_sources(&lines(&["#SIBD", "MSSTEREO"])));
        assert!(!mixed_sources(&lines(&["SITV", "MSSTEREO"])));
        assert!(!mixed_sources(&[]));
    }

    #[test]
    fn lines_of_the_file_are_trimmed_on_load() {
        let (kept, left_out) = load("SITV  \n#SIBD \nSI \nMSSTEREO \nPSDEL 000 \nMSDOLBY \n");
        assert_eq!(vec!["#SIBD", "SITV", "SI", "MSSTEREO", "PSDEL 000"], kept);
        assert_eq!(vec!["MSDOLBY"], left_out, "second answer");
        let (kept, left_out) = load("");
        assert!(kept.is_empty() && left_out.is_empty());
    }

    #[test]
    fn lines_of_the_file_that_set_no_saved_value_are_left_out() {
        let (kept, left_out) =
            load("SITV\nMV80\nPWON\n\n  \nCVEND\nMSQUICK1\n#MSSTEREO\n##SIBD\nPSBAS 50\n#SI\n");
        assert_eq!(vec!["#SI", "SITV", "PSBAS 50"], kept);
        assert!(mixed_sources(&kept), "refused");
        assert_eq!(
            vec!["MV80", "PWON", "CVEND", "MSQUICK1", "#MSSTEREO", "##SIBD"],
            left_out
        );
    }

    #[test]
    fn lines_of_the_file_are_loaded_in_the_order_of_the_queries() {
        let (kept, left_out) =
            load("PSBAS 50\nCVFR 50\nMSSTEREO\nSITV\nPSTRE 50\nCVFL 50\n#SIBD\n");
        assert_eq!(
            vec![
                "#SIBD", "SITV", "MSSTEREO", "PSBAS 50", "PSTRE 50", "CVFR 50", "CVFL 50"
            ],
            kept,
            "the channel lines keep their order"
        );
        assert!(left_out.is_empty());
    }

    #[test]
    fn all_zone_stereo_on_is_left_out_of_the_file() {
        let (kept, left_out) = load("SITV\nMNZST ON\nPSBAS 50\n");
        assert_eq!(vec!["SITV", "PSBAS 50"], kept);
        assert_eq!(vec!["MNZST ON"], left_out);
        let (kept, left_out) = load("SITV\nMNZST OFF\n");
        assert_eq!(vec!["SITV", "MNZST OFF"], kept);
        assert!(left_out.is_empty());
        let (kept, left_out) = load("SITV\nMNZST  ON\n");
        assert_eq!(vec!["SITV"], kept, "another spelling");
        assert_eq!(vec!["MNZST  ON"], left_out);
        let (kept, left_out) = load("SITV\nMNZST ON \n");
        assert_eq!(vec!["SITV"], kept);
        assert_eq!(vec!["MNZST ON"], left_out);
    }

    #[test]
    fn only_the_first_answer_to_a_query_is_loaded() {
        let (kept, left_out) = load("SITV\nPSBAS 50\nPSBAS 48\nCVFL 50\nCVFR 50\nPSTRE 50\n");
        assert_eq!(
            vec!["SITV", "PSBAS 50", "PSTRE 50", "CVFL 50", "CVFR 50"],
            kept,
            "in the order of the queries"
        );
        assert_eq!(vec!["PSBAS 48"], left_out);
        // All source lines stay, so that two source codes refuse the file.
        let (kept, left_out) = load("SITV\nSIBD\nMSSTEREO\n");
        assert_eq!(vec!["SITV", "SIBD", "MSSTEREO"], kept);
        assert!(left_out.is_empty());
        assert_eq!(None, saved_source(&kept));
    }

    #[test]
    fn a_line_that_is_left_out_counts_as_the_answer_of_its_kind() {
        let (kept, left_out) = load("SITV\nMNZST ON\nMNZST OFF\nPSBAS 50\n");
        assert_eq!(vec!["SITV", "PSBAS 50"], kept);
        assert_eq!(vec!["MNZST ON", "MNZST OFF"], left_out);
        let (kept, left_out) = load("SITV\n#MSSTEREO\nMSDOLBY AUDIO-DSUR\nPSBAS 50\n");
        assert_eq!(vec!["SITV", "PSBAS 50"], kept);
        assert_eq!(vec!["#MSSTEREO", "MSDOLBY AUDIO-DSUR"], left_out);
        let (kept, left_out) = load("SITV\n#CVFL 50\nCVFL 48\nCVFR 50\n");
        assert_eq!(vec!["SITV", "CVFR 50"], kept, "per channel");
        assert_eq!(vec!["#CVFL 50", "CVFL 48"], left_out);
        let (kept, left_out) = load("SITV\nMSQUICK1\nMSSTEREO\nSSTTR END\nSSTTR ON\n");
        assert_eq!(vec!["SITV", "MSSTEREO", "SSTTR ON"], kept, "no value");
        assert_eq!(vec!["MSQUICK1", "SSTTR END"], left_out);
    }

    #[test]
    fn a_value_that_changed_during_the_read_is_found() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let read = lines(&[
            "SITV", "MSSTEREO", "PSBAS 50", "PSBAS 52", "CVFL 50", "CVFR 50",
        ]);
        assert_eq!(vec![("PSBAS 50", "PSBAS 52")], changed_values(&read));
        let read = lines(&["SITV", "BTTX ON", "BTTX SP", "CVTTR OFF", "CVTTR 50"]);
        assert!(changed_values(&read).is_empty(), "on or off and a value");
        let read = lines(&["BTTX ON", "BTTX OFF", "CVFL 50", "CVFL 48"]);
        assert_eq!(
            vec![("BTTX ON", "BTTX OFF"), ("CVFL 50", "CVFL 48")],
            changed_values(&read)
        );
        // Two source codes are handled by `saved_source` and `taken`.
        assert!(changed_values(&lines(&["SITV", "SIBD", "MSSTEREO"])).is_empty());
        assert!(changed_values(&[]).is_empty());
    }

    #[test]
    fn the_transducer_on_line_and_level_line_are_both_loaded() {
        let (kept, left_out) = load("SITV\nCVTTR ON\nCVTTR 50\nCVTTR OFF\nCVTTR 48\n");
        assert_eq!(vec!["SITV", "CVTTR ON", "CVTTR 50"], kept);
        assert_eq!(vec!["CVTTR OFF", "CVTTR 48"], left_out);
    }

    #[test]
    fn only_the_first_line_of_each_channel_is_loaded() {
        let (kept, left_out) = load("SITV\nCVFL 50\nCVFR 50\nCVFL 48\nCVC 50\n");
        assert_eq!(vec!["SITV", "CVFL 50", "CVFR 50", "CVC 50"], kept);
        assert_eq!(vec!["CVFL 48"], left_out);
    }

    #[test]
    fn a_file_without_a_source_line_is_refused() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(source_lines_refused(&lines(&["PSBAS 50", "MSSTEREO"])).is_some());
        assert!(source_lines_refused(&[]).is_some());
        assert!(
            source_lines_refused(&lines(&["#SIBD", "SITV"])).is_some(),
            "mixed"
        );
        assert_eq!(None, source_lines_refused(&lines(&["SITV", "PSBAS 50"])));
        assert_eq!(None, source_lines_refused(&lines(&["#SIBD", "PSBAS 50"])));
        // A blank source line passes here and is refused later: no single saved source.
        assert_eq!(None, source_lines_refused(&lines(&["SI", "PSBAS 50"])));
    }

    #[test]
    fn only_lines_that_can_be_sent_are_counted() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(2, sent_count(&lines(&["SITV", "PSBAS 50"])));
        assert_eq!(1, sent_count(&lines(&["#SIBD", "PSBAS 50"])));
        assert_eq!(2, sent_count(&lines(&["SITV", "SI", "SI  ", "MSSTEREO"])));
        assert_eq!(0, sent_count(&[]));
    }

    #[test]
    fn a_source_kept_by_its_display_name_is_the_saved_source() {
        assert!(is_source_now(Some("TV"), "TV"));
        assert!(is_source_now(Some("Internet Radio"), "IRADIO"));
        assert!(!is_source_now(Some("Internet Radio"), "TV"));
        assert!(!is_source_now(Some("TV"), "IRADIO"));
        assert!(!is_source_now(None, "TV"));
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
        assert!(!is_saved("MSSMART1"));
        assert!(!is_saved("MSSMART2 MEMORY"));
        assert!(!is_saved("MVMAX 98"));
        assert!(is_saved("VSAUDIO TV"));
        assert!(is_saved("BTTX SP"));
        assert!(!is_saved("SSTTR END"));
        assert!(is_saved("SSTTR ON"));
        assert!(sleep_counts("SLP045"));
        assert!(!sleep_counts("SLPOFF"));
        let mut lines = vec!["SLP045".to_owned(), "PSBAS 50".to_owned()];
        leave_out_counting_sleep(&mut lines);
        assert_eq!(vec!["PSBAS 50"], lines);
        assert_eq!(
            (vec!["PSBAS 50".to_owned()], vec!["SLP045".to_owned()]),
            load("SLP045\nPSBAS 50\n"),
            "a counting sleep timer is left out"
        );
    }

    #[test]
    fn both_bluetooth_answer_lines_are_loaded() {
        let (kept, left_out) = load("SITV\nBTTX OFF\nBTTX SP\nBTTX ON\nBTTX BT\n");
        assert_eq!(vec!["SITV", "BTTX OFF", "BTTX SP"], kept);
        assert_eq!(vec!["BTTX ON", "BTTX BT"], left_out);
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
