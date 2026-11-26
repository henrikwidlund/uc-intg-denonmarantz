// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The signal sensors with an HDR and an SDR source.
//!
//! In telnet mode the receiver sends the answers to the queries of the second connection to the
//! driver too. The driver can then show a change at once, also without its own signal check.

use super::prompt::{Answer, Ask, ask_hint, terminal};
use super::{
    DRIVER_SLACK_SECS, HTTP_POLL_SECS, MAX_VOLUME, SetBack, WEB_PAGE_VALUES, above_baseline,
    cap_volume, config_device, ensure_on, entity_ids, first_error, highest_volume, lower_volume,
    norm, read_config, read_sources, receiver_volume, shown, source_back, strings, unmute_limit,
};
use crate::oracle;
use crate::{Check, Ctx, Fail};
use denon_marantz::proto::SignalState;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// The sensors that the check compares.
const SIGNAL_SENSORS: [&str; 6] = [
    "colorspace_input",
    "colorspace_output",
    "hdr_input",
    "hdr_output",
    "pixel_depth_input",
    "pixel_depth_output",
];

/// Signal time without a value in the configuration: `DEFAULT_SIGNAL_SECS` of the driver
/// (`src/config_store.rs`).
const DEFAULT_SIGNAL_SECS: u64 = 5;
/// Lowest signal time above 0: `MIN_SIGNAL_SECS` of the driver.
const MIN_SIGNAL_SECS: u64 = 3;
/// How long the second connection is watched for the signal that the owner started.
const ORACLE_SECS: u64 = 20;
/// Time between two signal reads of the second connection while it waits for the signal.
const ORACLE_REFRESH: Duration = Duration::from_secs(3);

/// The keys of the first prompt for a signal kind.
const KEYS: Ask = Ask::Action("it plays", "skip the checks that need it");
/// The keys of a prompt after the signal was not seen.
const KEYS_AGAIN: Ask = Ask::Action("check again", "skip the checks that need it");

/// The kind of signal that the owner plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want {
    Hdr,
    Sdr,
}

impl Want {
    const fn label(self) -> &'static str {
        match self {
            Self::Hdr => "HDR",
            Self::Sdr => "SDR",
        }
    }

    const fn prompt(self) -> &'static str {
        match self {
            Self::Hdr => "Play an HDR10 or Dolby Vision source on the receiver.",
            Self::Sdr => "Play an SDR video source over HDMI on the receiver.",
        }
    }

    /// The second connection has this kind of signal. HDR: a side has BT.2020 or an HDR format.
    /// The HDR format alone is enough: a player that does the Dolby Vision itself sends no BT.2020.
    /// SDR: a side has a color space, and no side has BT.2020 or an HDR format. SDR also needs no
    /// HDR format: on a receiver that reports BT.2020, BT.2020 is off for a short time after each
    /// change.
    fn seen(self, signal: &SignalState) -> bool {
        let color = signal.video_color();
        let sides = [color.input, color.output];
        let bt2020 = sides.iter().flatten().any(|c| c.bt2020);
        let hdr = signal.hdr.input.is_some() || signal.hdr.output.is_some();
        match self {
            Self::Hdr => bt2020 || hdr,
            Self::Sdr => sides.iter().any(Option::is_some) && !bt2020 && !hdr,
        }
    }
}

/// The signal of the second connection for a prompt or a note, for example
/// `4:2:2/BT.2020 HDR10 in, 4:2:2 no HDR out`.
fn describe(signal: &SignalState) -> String {
    let color = signal.video_color();
    let mut text = String::new();
    for (side, color, hdr) in [
        ("in", color.input, signal.hdr.input.as_deref()),
        ("out", color.output, signal.hdr.output.as_deref()),
    ] {
        if !text.is_empty() {
            text.push_str(", ");
        }
        match color {
            Some(color) => _ = write!(text, "{color}"),
            None => text.push_str("--"),
        }
        _ = write!(text, " {} {side}", hdr.unwrap_or("no HDR"));
    }
    text
}

/// Seconds between the signal checks of the driver for the HDR, color space and pixel depth
/// sensors. `None` if the driver does not check: in telnet mode it then shows only what the
/// receiver reports, in HTTP mode it does not read these values again.
/// `poll` is `signal_poll_secs` of `config.json`, see `DeviceConfig::signal_secs` of the driver.
/// `checks` is true if the library checks the signal on telnet (`Info::has_heos`). In HTTP mode
/// the time is rounded up to a multiple of [`HTTP_POLL_SECS`], and 0 does not read the page with
/// these values.
fn check_secs(poll: &Value, telnet: bool, checks: bool) -> Option<u64> {
    let secs = match poll.as_u64() {
        None => DEFAULT_SIGNAL_SECS,
        Some(0) => return None,
        Some(secs) => secs.max(MIN_SIGNAL_SECS),
    };
    if telnet {
        checks.then_some(secs)
    } else {
        Some(secs.div_ceil(HTTP_POLL_SECS) * HTTP_POLL_SECS)
    }
}

/// The longest time from the change on the second connection to the change in the driver.
fn driver_secs(check: Option<u64>) -> u64 {
    check.unwrap_or(0) + DRIVER_SLACK_SECS
}

/// The signal sensors that differ from the second connection.
fn differences(ctx: &Ctx, offered: &BTreeSet<String>) -> Vec<String> {
    let Some(oracle) = &ctx.oracle else {
        return Vec::new();
    };
    let mut bad = Vec::new();
    for (name, expected) in oracle::expected(&oracle.state()) {
        let Some(kind) = name.strip_prefix("sensor.") else {
            continue;
        };
        if !SIGNAL_SENSORS.contains(&kind) {
            continue;
        }
        let id = format!("{name}.{}", ctx.key);
        if !offered.contains(&id) {
            continue;
        }
        // The same rule as the other checks: a web page that failed gives no value. HDR is always
        // compared, also if WEB_PAGE_VALUES lists it: it has no value with SDR.
        if expected.is_none() && WEB_PAGE_VALUES.contains(&name) && !kind.starts_with("hdr_") {
            continue;
        }
        let got = shown(ctx.client.attr(&id, "value"));
        let want = norm(expected.as_deref().unwrap_or("--"));
        if got != want {
            bad.push(format!("{name}: shows {got}, receiver has {want}"));
        }
    }
    bad
}

/// When the second connection had the signal.
struct Seen {
    /// The time the check saw it. It can be late by the time of a signal read of the second
    /// connection, which the check waits for.
    at: Instant,
    /// It had the signal at the first look, right after the owner pressed Enter.
    at_enter: bool,
}

/// Wait until the second connection has the signal `want`. It reads the signal again every
/// [`ORACLE_REFRESH`]. `None` after [`ORACLE_SECS`].
async fn wait_oracle(ctx: &mut Ctx, want: Want) -> Result<Option<Seen>, Fail> {
    let deadline = Instant::now() + Duration::from_secs(ORACLE_SECS);
    let mut next_read = Instant::now();
    let mut at_enter = true;
    loop {
        let Some(oracle) = &ctx.oracle else {
            return Err(Fail::skip("no second connection"));
        };
        if want.seen(&oracle.state().device().signal) {
            return Ok(Some(Seen {
                at: Instant::now(),
                at_enter,
            }));
        }
        at_enter = false;
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        if now >= next_read {
            oracle.avr().refresh_signal().await?;
            next_read = now + ORACLE_REFRESH;
        }
        ctx.client.listen_ms(300).await?;
    }
}

/// Wait until the driver shows the signal of the second connection. `Ok` with the time since
/// `from`, `Err` with the differences `secs` after `from`.
async fn wait_driver(
    ctx: &mut Ctx,
    offered: &BTreeSet<String>,
    from: Instant,
    secs: u64,
) -> Result<Result<Duration, Vec<String>>, Fail> {
    let limit = Duration::from_secs(secs);
    loop {
        let bad = differences(ctx, offered);
        if bad.is_empty() {
            return Ok(Ok(from.elapsed()));
        }
        if from.elapsed() >= limit {
            return Ok(Err(bad));
        }
        ctx.client.listen_ms(300).await?;
    }
}

/// The result of one signal kind.
enum Part {
    /// The driver showed the signal, or it does not check these values. The text is for the note.
    Shown(String),
    /// Not checked, with the reason.
    Skipped(String),
    /// A failure: the driver did not show the signal, or the volume went above [`MAX_VOLUME`].
    Different(String),
}

/// Ask the owner to play `want`, wait for it on the second connection and then for the driver.
/// After Enter or s, a volume above [`MAX_VOLUME`] is lowered with [`cap_volume`]
/// and the kind fails; a volume that was not lowered is an error. `hint` goes under each prompt.
/// `stop` is set when the input ended: then `check_one` asks no more questions.
async fn check_one(
    ctx: &mut Ctx,
    want: Want,
    offered: &BTreeSet<String>,
    check: Option<u64>,
    hint: Option<&str>,
    stop: &mut bool,
) -> Result<Part, Fail> {
    let label = want.label();
    if *stop {
        return Ok(Part::Skipped(format!("{label}: not asked (no input)")));
    }
    let mut keys = KEYS;
    let mut text = want.prompt().to_owned();
    let seen = loop {
        match ask_hint(ctx, keys, &text, hint).await? {
            // The owner can change the volume with the source.
            Answer::Done => {
                if let Some(loud) = cap_volume(ctx).await? {
                    return Ok(Part::Different(format!("{label}: {loud}")));
                }
            }
            Answer::NoInput => {
                *stop = true;
                return Ok(Part::Skipped(format!("{label}: not started (no input)")));
            }
            // The next prompt must not start above the limit.
            _ => {
                if let Some(loud) = cap_volume(ctx).await? {
                    return Ok(Part::Different(format!("{label}: {loud}")));
                }
                return Ok(Part::Skipped(format!("{label}: the owner skipped")));
            }
        }
        if let Some(seen) = wait_oracle(ctx, want).await? {
            break seen;
        }
        let now = ctx
            .oracle
            .as_ref()
            .map(|o| describe(&o.state().device().signal))
            .unwrap_or_default();
        text = format!(
            "The receiver does not show {label} after {ORACLE_SECS} s, it shows {now}. {}",
            want.prompt()
        );
        keys = KEYS_AGAIN;
    };
    let secs = driver_secs(check);
    let receiver = ctx
        .oracle
        .as_ref()
        .map(|o| describe(&o.state().device().signal))
        .unwrap_or_default();
    // At the first look the signal was there before Enter: the time of the change is not known.
    let from = if seen.at_enter {
        "Enter, the receiver showed it at Enter"
    } else {
        "the receiver"
    };
    match wait_driver(ctx, offered, seen.at, secs).await? {
        // 0 ms: the driver can also have changed before the second connection saw the change.
        Ok(took) => Ok(Part::Shown(format!(
            "{label} ({receiver}): driver {} ms after {from} (limit {secs} s)",
            took.as_millis()
        ))),
        // Without a signal check the driver shows only what the receiver reports (telnet), or
        // keeps its values (HTTP).
        Err(bad) if check.is_none() => Ok(Part::Shown(format!(
            "{label} ({receiver}): not shown in {secs} s, the driver does not check these values: [{}]",
            bad.join(", ")
        ))),
        Err(bad) => Ok(Part::Different(format!(
            "{label} ({receiver}): not shown in {secs} s: [{}]",
            bad.join(", ")
        ))),
    }
}

pub async fn signal(ctx: &mut Ctx) -> Check {
    let Some(oracle) = &ctx.oracle else {
        return Err(Fail::skip("no second connection"));
    };
    let has_check = oracle.avr().info().has_heos();
    if !terminal() {
        return Err(Fail::skip(
            "not asked: no terminal, the owner must change the source",
        ));
    }
    let offered = entity_ids(ctx).await?;
    if !SIGNAL_SENSORS
        .iter()
        .any(|k| offered.contains(&ctx.sensor(k)))
    {
        return Err(Fail::skip("the driver has no signal sensors"));
    }
    ensure_on(ctx).await?;
    let volume = highest_volume(ctx).await?;
    if volume > MAX_VOLUME {
        return Err(Fail::skip(format!(
            "the volume {volume} is above {MAX_VOLUME}"
        )));
    }
    // The owner changes the source: the input levels must not make another source louder.
    if !ctx.opt.avoid_sources.is_empty() {
        read_sources(ctx)
            .await
            .map_err(|e| Fail::skip(format!("not asked: {e}")))?;
    }
    let hint = avoid_hint(&ctx.opt.avoid_sources);
    let config = read_config(ctx).await?;
    let device = config_device(ctx, &config)?;
    let telnet = device["use_telnet"] != false;
    let check = check_secs(&device["signal_poll_secs"], telnet, has_check);
    let mp = ctx.mp();
    let old = ctx.client.text(&mp, "source");
    if old.is_empty() {
        return Err(Fail::skip(
            "the driver shows no source: it could not be set back",
        ));
    }
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    let mut stop = false;
    let body = async {
        let hint = hint.as_deref();
        let hdr = check_one(ctx, Want::Hdr, &offered, check, hint, &mut stop).await?;
        let sdr = check_one(ctx, Want::Sdr, &offered, check, hint, &mut stop).await?;
        Ok::<_, Fail>([hdr, sdr])
    }
    .await;
    // The owner can change the volume also before s, at the end of the input or before an error.
    // A volume that was not lowered is a failure too: the set back then asks the owner.
    // The cap lowers only to the limit, so the start volume is set back after it.
    let capped = cap_volume(ctx).await.unwrap_or_else(|e| Some(e.msg));
    let loud = match (capped, rise_back(ctx, volume).await) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    };
    let back = source_back(ctx, &old, &list, volume).await;
    join(check, body, loud, back)
}

/// Set the receiver volume back to `start`, the volume before the prompts, if it is above it.
/// `Some` with the values if it was above (the check fails with this text), or with the error if
/// it could not be read. `None` if it is not above.
async fn rise_back(ctx: &mut Ctx, start: f64) -> Option<String> {
    let now = match receiver_volume(ctx).await {
        Ok(Some(now)) => now,
        Ok(None) => return None,
        Err(e) => return Some(e.msg),
    };
    if !above_baseline(now, Some(start)) {
        return None;
    }
    let limit = unmute_limit(Some(start));
    let back = match lower_volume(ctx, Some(start)).await {
        Ok(()) => format!("set back to {limit}"),
        Err(e) => format!("not set back to {limit}: {e}"),
    };
    Some(format!(
        "the receiver volume {now} after the prompts is above the start volume {start}, {back}"
    ))
}

/// The hint line of the prompts: the sources that the owner must not use. `None` without them.
fn avoid_hint(avoid: &[String]) -> Option<String> {
    (!avoid.is_empty()).then(|| format!("Do not use: {}", avoid.join(", ")))
}

/// The result of the step from the result of the check, the text `loud` of the volume check after
/// the prompts (a failure, see [`cap_volume`] and [`rise_back`]) and the result of the set back.
fn join(
    check: Option<u64>,
    body: Result<[Part; 2], Fail>,
    loud: Option<String>,
    back: Result<SetBack, Fail>,
) -> Check {
    let body = match (body, &loud) {
        (Err(mut e), Some(loud)) => {
            e.msg.push_str("; ");
            e.msg.push_str(loud);
            e.skip = false;
            Err(e)
        }
        (body, _) => body,
    };
    match back {
        Ok(back) => match body {
            Ok(parts) => verdict(check, parts, loud, back),
            // The error of the check, with the note of the set back.
            Err(mut e) if !back.note.is_empty() => {
                e.msg.push_str("; ");
                e.msg.push_str(&back.note);
                Err(e)
            }
            Err(e) => Err(e),
        },
        // The notes of the parts, with the error of the set back.
        Err(e) => match body {
            Ok(parts) => {
                let (Ok(note) | Err(Fail { msg: note, .. })) =
                    verdict(check, parts, loud, SetBack::note(String::new()));
                Err(Fail::new(format!("{note}; not set back: {e}")))
            }
            body => first_error(body, Err(e)).map(|_| String::new()),
        },
    }
}

/// The result of the step: FAIL if the driver did not show a signal within its signal check time
/// plus [`DRIVER_SLACK_SECS`] (without a signal check there is no FAIL for this), or if the volume
/// went above [`MAX_VOLUME`] after a prompt, or above the start volume after the prompts (`loud`).
/// SKIP if both kinds or the set back were skipped, else PASS.
fn verdict(check: Option<u64>, parts: [Part; 2], loud: Option<String>, back: SetBack) -> Check {
    let mut notes = vec![match check {
        Some(secs) => format!("signal check every {secs} s"),
        None => "no signal check".to_owned(),
    }];
    let (mut different, mut skipped) = (false, 0);
    for part in parts {
        notes.push(match part {
            Part::Shown(text) => text,
            Part::Skipped(text) => {
                skipped += 1;
                text
            }
            Part::Different(text) => {
                different = true;
                text
            }
        });
    }
    if let Some(loud) = loud {
        different = true;
        notes.push(loud);
    }
    if !back.note.is_empty() {
        notes.push(back.note);
    }
    let note = notes.join("; ");
    if different {
        Err(Fail::new(note))
    } else if skipped == 2 || back.skipped {
        Err(Fail::skip(note))
    } else {
        Ok(note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denon_marantz::State;
    use denon_marantz::proto::telnet::Message;
    use serde_json::json;

    fn signal(lines: &[&str]) -> SignalState {
        let mut state = State::new(true);
        for line in lines {
            let message = Message::parse(line).unwrap_or_else(|| panic!("not a message: {line}"));
            state.apply_message(&message);
        }
        state.device().signal.clone()
    }

    #[test]
    fn hdr_is_seen_with_bt2020() {
        // A receiver without `SSINFSIGECI`: HDR means BT.2020.
        let hdr = signal(&["SSINFSIGCOS 11", "SSINFSIGHDR IHDR10"]);
        assert!(Want::Hdr.seen(&hdr));
        assert!(!Want::Sdr.seen(&hdr));
        // A receiver with `SSINFSIGECI`: BT.2020 only when it says so. No HDR format here.
        let reported = signal(&["SSINFSIGECI 55", "SSINFSIGCOS 11"]);
        assert!(
            !Want::Hdr.seen(&reported),
            "BT.2020 not known after the change"
        );
        let reported = signal(&["SSINFSIGECI 00", "SSINFSIGCOS 11", "SSINFSIGECI 55"]);
        assert!(Want::Hdr.seen(&reported), "BT.2020 alone");
        let output = signal(&["SSINFSIGECI 00", "SSINFSIGCOS 22", "SSINFSIGECI 05"]);
        assert!(Want::Hdr.seen(&output), "BT.2020 on the output only");
    }

    #[test]
    fn hdr_is_seen_with_an_hdr_format_alone() {
        // Dolby Vision from the player: no BT.2020.
        let dolby = signal(&[
            "SSINFSIGECI 00",
            "SSINFSIGCOS 22",
            "SSINFSIGHDR IDolby Vision",
        ]);
        assert!(Want::Hdr.seen(&dolby));
        assert!(!Want::Sdr.seen(&dolby));
        let output = signal(&["SSINFSIGHDR OHDR10"]);
        assert!(Want::Hdr.seen(&output), "HDR format on the output only");
        // BT.2020 off for a short time after a change, while HDR still plays.
        let changing = signal(&["SSINFSIGECI 00", "SSINFSIGCOS 11", "SSINFSIGHDR IHDR10"]);
        assert!(Want::Hdr.seen(&changing));
        let off = signal(&["SSINFSIGHDR IHDR10", "SSINFSIGHDR I---"]);
        assert!(!Want::Hdr.seen(&off), "--- is no HDR format");
    }

    #[test]
    fn sdr_needs_a_color_space_without_bt2020_and_hdr() {
        assert!(!Want::Sdr.seen(&signal(&[])), "no video");
        assert!(!Want::Hdr.seen(&signal(&[])), "no video");
        let sdr = signal(&["SSINFSIGCOS 22", "SSINFSIGHDR I---", "SSINFSIGHDR O---"]);
        assert!(Want::Sdr.seen(&sdr));
        assert!(!Want::Hdr.seen(&sdr));
        // BT.2020 is off for a short time after a change, while HDR still plays.
        let changing = signal(&["SSINFSIGECI 00", "SSINFSIGCOS 11", "SSINFSIGHDR IHDR10"]);
        assert!(!Want::Sdr.seen(&changing));
        let output_only = signal(&["SSINFSIGCOS 03"]);
        assert!(Want::Sdr.seen(&output_only));
        let hdr_out = signal(&["SSINFSIGECI 00", "SSINFSIGCOS 22", "SSINFSIGHDR OHDR10"]);
        assert!(!Want::Sdr.seen(&hdr_out), "HDR on the output only");
        assert!(Want::Hdr.seen(&hdr_out));
        // HDR to SDR on a receiver that reports BT.2020.
        let after_hdr = signal(&[
            "SSINFSIGECI 55",
            "SSINFSIGCOS 11",
            "SSINFSIGHDR IHDR10",
            "SSINFSIGHDR I---",
            "SSINFSIGCOS 22",
            "SSINFSIGECI 00",
        ]);
        assert!(Want::Sdr.seen(&after_hdr));
        assert!(!Want::Hdr.seen(&after_hdr));
    }

    #[test]
    fn the_description_has_both_sides() {
        assert_eq!(
            "4:2:2/BT.2020 HDR10 in, 4:2:2 no HDR out",
            describe(&signal(&["SSINFSIGCOS 11", "SSINFSIGHDR IHDR10"]))
        );
        assert_eq!("-- no HDR in, -- no HDR out", describe(&signal(&[])));
    }

    #[test]
    fn the_check_time_follows_the_driver() {
        let null = Value::Null;
        // Telnet with the library check.
        // A missing field reads as null.
        assert_eq!(Some(5), check_secs(&null, true, true), "default");
        assert_eq!(None, check_secs(&json!(0), true, true), "off");
        assert_eq!(Some(3), check_secs(&json!(1), true, true), "lowest");
        assert_eq!(Some(30), check_secs(&json!(30), true, true));
        // Older receivers have no check.
        assert_eq!(None, check_secs(&null, true, false));
        assert_eq!(None, check_secs(&json!(30), true, false));
        // HTTP mode: in steps of the poll time.
        assert_eq!(Some(10), check_secs(&null, false, false));
        assert_eq!(Some(10), check_secs(&json!(1), false, true));
        assert_eq!(Some(10), check_secs(&json!(10), false, true));
        assert_eq!(Some(20), check_secs(&json!(11), false, true));
        assert_eq!(None, check_secs(&json!(0), false, true), "no signal pages");
    }

    #[test]
    fn the_hint_lists_the_sources_to_avoid() {
        assert_eq!(None, avoid_hint(&[]));
        assert_eq!(
            Some("Do not use: oppo, bluray".to_owned()),
            avoid_hint(&["oppo".to_owned(), "bluray".to_owned()])
        );
    }

    #[test]
    fn the_driver_gets_five_seconds_more() {
        assert_eq!(10, driver_secs(Some(5)));
        assert_eq!(25, driver_secs(Some(20)));
        assert_eq!(5, driver_secs(None));
    }

    fn shown_part() -> Part {
        Part::Shown("shown".to_owned())
    }

    fn skipped_part() -> Part {
        Part::Skipped("skipped".to_owned())
    }

    fn back(note: &str, skipped: bool) -> SetBack {
        SetBack {
            note: note.to_owned(),
            skipped,
        }
    }

    #[test]
    fn join_keeps_the_error_and_the_notes() {
        let parts = || Ok([shown_part(), shown_part()]);
        fn error<T>() -> Result<T, Fail> {
            Err(Fail::new("error"))
        }
        assert_eq!(
            Ok("signal check every 5 s; shown; shown; set back".to_owned()),
            join(Some(5), parts(), None, Ok(back("set back", false))).map_err(|e| e.msg)
        );
        let body_error = join(Some(5), error(), None, Ok(back("set back", false))).unwrap_err();
        assert_eq!(
            ("error; set back", false),
            (body_error.msg.as_str(), body_error.skip)
        );
        let no_note = join(Some(5), error(), None, Ok(back("", false))).unwrap_err();
        assert_eq!("error", no_note.msg);
        let back_error = join(Some(5), parts(), None, error()).unwrap_err();
        assert_eq!(
            (
                "signal check every 5 s; shown; shown; not set back: error",
                false
            ),
            (back_error.msg.as_str(), back_error.skip)
        );
        let both = join(Some(5), Err(Fail::skip("body")), None, error()).unwrap_err();
        assert_eq!(
            ("body; not set back: error", false),
            (both.msg.as_str(), both.skip)
        );
        // A failed set back fails also when both kinds were skipped.
        let skipped =
            join(Some(5), Ok([skipped_part(), skipped_part()]), None, error()).unwrap_err();
        assert!(!skipped.skip);
        assert!(skipped.msg.ends_with("; not set back: error"));
    }

    #[test]
    fn the_verdict_follows_the_parts_and_the_set_back() {
        let none = || back("", false);
        assert_eq!(
            Ok("signal check every 5 s; shown; shown".to_owned()),
            verdict(Some(5), [shown_part(), shown_part()], None, none()).map_err(|e| e.msg)
        );
        let fail = verdict(
            None,
            [Part::Different("different".to_owned()), skipped_part()],
            None,
            back("set back", true),
        )
        .unwrap_err();
        assert!(!fail.skip, "a difference fails, also with skips");
        assert_eq!("no signal check; different; skipped; set back", fail.msg);
        let both = verdict(Some(5), [skipped_part(), skipped_part()], None, none()).unwrap_err();
        assert!(both.skip);
        let one = verdict(Some(5), [skipped_part(), shown_part()], None, none());
        assert!(one.is_ok(), "one kind is enough");
        let back_skipped = verdict(
            Some(5),
            [shown_part(), shown_part()],
            None,
            back("not set back", true),
        );
        assert!(back_skipped.is_err_and(|e| e.skip));
    }

    #[test]
    fn a_loud_volume_after_the_prompts_fails() {
        let loud = || Some("loud".to_owned());
        let fail = verdict(
            Some(5),
            [skipped_part(), skipped_part()],
            loud(),
            back("set back", false),
        )
        .unwrap_err();
        assert_eq!(
            (
                "signal check every 5 s; skipped; skipped; loud; set back",
                false
            ),
            (fail.msg.as_str(), fail.skip)
        );
        let body_skip = join(
            Some(5),
            Err(Fail::skip("no input")),
            loud(),
            Ok(back("set back", false)),
        )
        .unwrap_err();
        assert_eq!(
            ("no input; loud; set back", false),
            (body_skip.msg.as_str(), body_skip.skip)
        );
        let back_error = join(
            Some(5),
            Ok([shown_part(), shown_part()]),
            loud(),
            Err(Fail::new("error")),
        )
        .unwrap_err();
        assert_eq!(
            "signal check every 5 s; shown; shown; loud; not set back: error",
            back_error.msg
        );
    }
}
