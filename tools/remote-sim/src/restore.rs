// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Saves the receiver settings that the `simple_commands` check changes, and sets them back. The
//! saved lines are also written to a file. If a run stops before the settings are set back, the
//! next run sets them back at its start.

use crate::Fail;
use crate::oracle::Oracle;
use denon_marantz::{Ask, Avr, Command, Zone};
use std::path::Path;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

/// The file with the saved settings. It exists only while settings are changed.
pub const FILE: &str = "remote-sim-restore.txt";

/// Queries of the settings, in the order in which they are set back. The sound mode goes first: it
/// can change the sound settings. The Dirac filter goes before MultEQ, and the tone control before
/// bass and treble.
pub const QUERIES: &[&str] = &[
    "MS?",
    "VSAUDIO ?",
    "VSMONI ?",
    "VSVPM ?",
    "BTTX ?",
    "SLP?",
    "MNZST?",
    "ECO?",
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
    "PSPAN ?",
    "PSSURLEV ?",
    "PSIMAXAUD ?",
    "PSIMAXSWM ?",
    "PSIMAXSWO ?",
    "PSAUROST ?",
    "PSAUROMODE ?",
    "PSDIC ?",
    "PSDELAY ?",
    "PSDEL ?",
    "PSRSZ ?",
    "PSTONE CTRL ?",
    "PSBAS ?",
    "PSTRE ?",
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

/// Send the saved lines, read with `queries`. Lines that did not come back are sent once more.
/// Returns the lines that the receiver did not take: they have a different value when read again.
pub async fn write(avr: &Avr, queries: &[&str], lines: &[String]) -> Result<Vec<String>, Fail> {
    for line in lines {
        avr.telnet_send(Zone::Main, Command::Raw(line)).await?;
    }
    // The receiver needs time to apply a sound mode.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let now = read(avr, queries).await?;
    let mut missed: Vec<String> = lines
        .iter()
        .filter(|line| !now.contains(line))
        .cloned()
        .collect();
    if missed.is_empty() {
        return Ok(missed);
    }
    // A busy receiver can answer too late, or ignore a line while it applies the sound mode. Send
    // the missed lines again and ask for them only.
    for line in &missed {
        avr.telnet_send(Zone::Main, Command::Raw(line)).await?;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut again: Vec<&str> = Vec::new();
    for query in missed
        .iter()
        .filter_map(|line| position(line))
        .map(|i| QUERIES[i])
    {
        if !again.contains(&query) {
            again.push(query);
        }
    }
    let now = read(avr, &again).await?;
    missed.retain(|line| !now.contains(line));
    Ok(missed)
}

/// Write the saved lines to [`FILE`].
pub async fn save(lines: &[String]) -> Result<(), Fail> {
    tokio::fs::write(FILE, lines.join("\n") + "\n")
        .await
        .map_err(|e| Fail::new(format!("{FILE}: {e}")))
}

/// Skip a check that saves the settings while [`FILE`] still holds the settings of an earlier run:
/// the check would replace them.
pub fn ensure_nothing_left() -> Result<(), Fail> {
    if Path::new(FILE).exists() {
        return Err(Fail::skip(format!(
            "settings of the last run are not set back. They are in {FILE}"
        )));
    }
    Ok(())
}

/// Remove [`FILE`] after all settings are set back.
pub async fn forget() {
    let _ = tokio::fs::remove_file(FILE).await;
}

/// Set back the settings of a run that stopped early. Returns what was done, or `None` if there is
/// nothing to set back.
pub async fn after_early_stop(host: &str) -> Option<String> {
    let text = tokio::fs::read_to_string(FILE).await.ok()?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let result = async {
        let oracle = Oracle::connect(host).await?;
        let missed = write(oracle.avr(), QUERIES, &lines).await;
        oracle.close().await;
        missed
    }
    .await;
    Some(match result {
        Ok(missed) if missed.is_empty() => {
            forget().await;
            format!("{} settings of the last run set back", lines.len())
        }
        // The file stays: the next run tries again.
        Ok(missed) => format!(
            "settings of the last run set back, not taken: {missed:?}. They stay in {}",
            Path::new(FILE).display()
        ),
        Err(e) => format!(
            "settings of the last run not set back: {}. They are in {}",
            e.msg,
            Path::new(FILE).display()
        ),
    })
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
        assert_eq!(None, query("PWON"));
    }
}
