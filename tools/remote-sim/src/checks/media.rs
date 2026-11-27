// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The media info of the tuner and of Bluetooth.

use super::prompt::{Answer, Ask, ask, ask_hint, terminal};
use super::{
    SourceRule, TUNER_CODES, above_baseline, add_back_note, add_set_back, cap_volume,
    driver_volume, ensure_on, first_error, highest, label_fits, lower_volume, read_sources,
    receiver_volume, restore_source, sound_volume, source_back, source_rule, start_source_ok,
    strings, too_loud, unmute_limit,
};
use crate::{Check, Ctx, Fail};
use denon_marantz::Zone;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Empty value of a sensor or a select of the driver.
const EMPTY: &str = "--";

/// What s + Enter means at the play prompt and at the question of the Bluetooth part.
const SKIP_BLUETOOTH: &str = "skip the Bluetooth part";

/// What s + Enter means at the prompt to stop the music. It only adds a note.
const MUSIC_PLAYS: &str = "the music still plays";

/// Why a part that makes sound does not run.
const NO_SOUND: &str = "nothing that makes sound runs";

/// Seconds to wait for the media info or the Bluetooth source.
const MEDIA_SECS: u64 = 10;

/// Check the media info of the media player: the tuner shows the text of the driver rule (the title
/// of the receiver, else the frequency; the artist of the receiver, else the band), and the owner
/// checks what Bluetooth shows. The step fails if a part fails, and skips only if all parts skip.
pub async fn media_info(ctx: &mut Ctx) -> Check {
    // The tuner part selects a source and both parts set the start source back, so both need the
    // source rule.
    if ctx.opt.avoid_sources.is_empty() || ctx.oracle.is_none() {
        return Err(Fail::skip(
            "no --avoid-sources or no second connection: no source is selected",
        ));
    }
    if !ctx.tuner && !terminal() {
        return Err(Fail::skip(
            "the driver shows no tuner entities and there is no terminal",
        ));
    }
    ensure_on(ctx).await?;
    let tuner = Part::from(tuner_part(ctx).await);
    let bluetooth = Part::from(bluetooth_part(ctx).await);
    combine(&[("tuner", tuner), ("Bluetooth", bluetooth)])
}

/// Result of a part of the step.
#[derive(Debug, PartialEq)]
enum Part {
    Pass(String),
    Skip(String),
    Fail(String),
}

impl From<Check> for Part {
    fn from(result: Check) -> Self {
        match result {
            Ok(note) => Self::Pass(note),
            Err(f) if f.skip => Self::Skip(f.msg),
            Err(f) => Self::Fail(f.msg),
        }
    }
}

/// One note for all parts. Fails if a part fails, skips only if all parts skip.
fn combine(parts: &[(&str, Part)]) -> Check {
    let mut note = String::new();
    for (name, part) in parts {
        if !note.is_empty() {
            note.push_str("; ");
        }
        let _ = match part {
            Part::Pass(msg) => write!(note, "{name}: {msg}"),
            Part::Skip(msg) => write!(note, "{name} skipped: {msg}"),
            Part::Fail(msg) => write!(note, "{name} failed: {msg}"),
        };
    }
    if parts.iter().any(|(_, p)| matches!(p, Part::Fail(_))) {
        Err(Fail::new(note))
    } else if parts.iter().all(|(_, p)| matches!(p, Part::Skip(_))) {
        Err(Fail::skip(note))
    } else {
        Ok(note)
    }
}

/// The name in the driver list `list` of a tuner source that may be selected, or why there is
/// none. The start source `old` must be one that may be selected: the set back selects it again.
/// `TUNER` comes before `HDRADIO`. Only the selected source must pass the rule, because the check
/// selects it by its name.
fn tuner_source(rule: &SourceRule<'_>, old: &str, list: &[String]) -> Result<String, String> {
    start_source_ok(rule, old, list)?;
    let mut listed = false;
    let mut allowed = false;
    for code in TUNER_CODES {
        let Some(info) = rule.info(code) else {
            continue;
        };
        listed = true;
        if !rule.code_ok(code) {
            continue;
        }
        allowed = true;
        if let Some(label) = list
            .iter()
            .find(|l| label_fits(info, l) && rule.label_ok(l))
        {
            return Ok(label.clone());
        }
    }
    Err(if allowed {
        "no tuner source that may be selected is in the source list".to_owned()
    } else if listed {
        "no tuner source may be selected".to_owned()
    } else {
        "the receiver has no tuner source".to_owned()
    })
}

/// Select a tuner source, check the media info and set the start source back, also after a
/// failure.
async fn tuner_part(ctx: &mut Ctx) -> Check {
    if !ctx.tuner {
        return Err(Fail::skip("the driver shows no tuner entities"));
    }
    // The driver offers the frequency sensor only with telnet: without it, the text is not known.
    if ctx
        .client
        .attr(&ctx.sensor("tuner_frequency"), "value")
        .is_none()
    {
        return Err(Fail::skip(
            "the driver shows no tuner frequency sensor, it needs telnet",
        ));
    }
    let mp = ctx.mp();
    let old = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    let sources = read_sources(ctx).await.map_err(Fail::skip)?;
    let label = tuner_source(&source_rule(ctx, &sources), &old, &list).map_err(Fail::skip)?;
    if let Some(why) = too_loud(ctx).await? {
        return Err(Fail::skip(format!("{why}: {NO_SOUND}")));
    }
    if label == old {
        let note = tuner_shown(ctx, &label).await?;
        return Ok(format!("{note}, the tuner was the start source"));
    }
    let result = async {
        ctx.client
            .command_ok(
                "media_player",
                &mp,
                "select_source",
                json!({ "source": label }),
            )
            .await?;
        ctx.client
            .wait_text(&mp, "source", 8, &label)
            .await
            .map_err(|e| Fail::new(format!("{e}. Wanted: {label}, source list: {list:?}")))?;
        tuner_shown(ctx, &label).await
    }
    .await;
    let back = restore_source(ctx, &old, 4)
        .await
        .map(|tries| format!("source set back to {old} in {tries} tries"));
    add_back_note(result, back)
}

/// Wait until the media player shows the tuner text that the driver rule gives.
async fn tuner_shown(ctx: &mut Ctx, label: &str) -> Check {
    let mp = ctx.mp();
    let frequency_id = ctx.sensor("tuner_frequency");
    let band_id = ctx.select("tuner_band");
    let deadline = Instant::now() + Duration::from_secs(MEDIA_SECS);
    loop {
        let title = ctx.client.text(&mp, "media_title");
        let artist = ctx.client.text(&mp, "media_artist");
        let freq = ctx.client.text(&frequency_id, "value");
        let band = ctx.client.text(&band_id, "current_option");
        let (now_title, now_artist) = ctx.oracle.as_ref().map_or((None, None), |oracle| {
            let state = oracle.state();
            let now = &state.device().now_playing;
            (now.title.clone(), now.artist.clone())
        });
        let expected = tuner_expected(now_title.as_deref(), now_artist.as_deref(), &freq, &band);
        let Some(wrong) = tuner_mismatch(&title, &artist, expected) else {
            return Ok(format!("{label}: title {title:?}, artist {artist:?}"));
        };
        if Instant::now() >= deadline {
            return Err(Fail::new(format!(
                "{label}: {wrong} (after {MEDIA_SECS} s)"
            )));
        }
        ctx.client.listen_ms(200).await?;
    }
}

/// A value of a sensor or a select that is known.
fn known(value: &str) -> Option<&str> {
    (!value.is_empty() && value != EMPTY).then_some(value)
}

/// The title and the artist that the driver shows for the tuner: the title of the receiver, else
/// the frequency; the artist of the receiver, else the band. Empty if not known.
fn tuner_expected<'a>(
    now_title: Option<&'a str>,
    now_artist: Option<&'a str>,
    frequency: &'a str,
    band: &'a str,
) -> (&'a str, &'a str) {
    (
        now_title.or_else(|| known(frequency)).unwrap_or_default(),
        now_artist.or_else(|| known(band)).unwrap_or_default(),
    )
}

/// What is wrong with the shown `title` and `artist`, or `None` if they are right. A title that is
/// not known is wrong: the tuner always has a frequency.
fn tuner_mismatch(title: &str, artist: &str, expected: (&str, &str)) -> Option<String> {
    let (want_title, want_artist) = expected;
    if want_title.is_empty() {
        return Some(format!(
            "the frequency is not known, the title is {title:?}"
        ));
    }
    (title != want_title || artist != want_artist).then(|| {
        format!(
            "title {title:?} and artist {artist:?}, expected {want_title:?} and {want_artist:?}"
        )
    })
}

/// The owner plays music from a phone over Bluetooth and says if the media player shows it.
/// The source rule does not apply to the `BT` source: the owner selects it. At the end a receiver
/// volume above the start volume is set back, then the source is set back, then the owner stops
/// the music. A skipped set back of the source makes the part skip if nothing failed.
async fn bluetooth_part(ctx: &mut Ctx) -> Check {
    if !terminal() {
        return Err(Fail::skip(not_answered(Answer::Skip, false)));
    }
    // The input levels must pass `levels_ok`: a higher level makes the source louder.
    read_sources(ctx).await.map_err(Fail::skip)?;
    let Some(start) = receiver_volume(ctx).await? else {
        return Err(Fail::skip(
            "no second connection: the receiver volume is not known",
        ));
    };
    // The higher volume is the limit for the source set back, as in the other checks.
    let start_high = sound_volume(highest(Some(start), driver_volume(ctx).await?))
        .map_err(|why| Fail::skip(format!("{why}: {NO_SOUND}")))?;
    let mp = ctx.mp();
    let old = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    let start_bt = receiver_on_bt(ctx);
    let answer = ask(
        ctx,
        Ask::Action("it plays", SKIP_BLUETOOTH),
        "Play music from a phone over Bluetooth to the receiver. This makes sound.",
    )
    .await;
    // The owner may have started Bluetooth also before a skip or an error. The skip and error
    // paths do not wait in `bluetooth_shown`, so the source and the volume get 2 s to change
    // before they are compared.
    let (body, settle_ms) = match answer {
        Ok(Answer::Done) => (bluetooth_shown(ctx, &old, start_bt, start).await, 300),
        Ok(other) => (Err(Fail::skip(not_answered(other, true))), 2000),
        Err(e) => (Err(e), 2000),
    };
    // The volume goes back first: a source set back must not start at a volume that the phone set.
    let settle = ctx.client.listen_ms(settle_ms).await;
    let volume = first_error(bt_volume_back(ctx, start).await, settle);
    // `source_back` sends nothing if the receiver has the start source.
    let back = source_back(ctx, &old, &list, start_high).await;
    let stop = ask(
        ctx,
        Ask::Action("done", MUSIC_PLAYS),
        "Stop the music on the phone.",
    )
    .await;
    with_stop(with_volume(add_set_back(body, back), volume), stop)
}

/// Set the receiver volume back to `start`, the volume before the play prompt, if it is above it:
/// the phone can set the volume. A volume above `start` is a failure, also after it is set back.
/// The limit is never above [`MAX_VOLUME`](super::MAX_VOLUME).
async fn bt_volume_back(ctx: &mut Ctx, start: f64) -> Result<(), Fail> {
    let now = receiver_volume(ctx)
        .await?
        .ok_or_else(|| Fail::new("the receiver volume after Bluetooth is not known"))?;
    if !above_baseline(now, Some(start)) {
        return Ok(());
    }
    let limit = unmute_limit(Some(start));
    let back = match lower_volume(ctx, Some(start)).await {
        Ok(()) => format!("set back to {limit}"),
        Err(e) => format!("not set back to {limit}: {e}"),
    };
    Err(Fail::new(format!(
        "the receiver volume {now} after Bluetooth is above the start volume {start}, {back}"
    )))
}

/// Add the result `volume` of the volume check to `result`. A volume error is always a failure.
fn with_volume(result: Check, volume: Result<(), Fail>) -> Check {
    match (result, volume) {
        (result, Ok(())) => result,
        (Ok(note), Err(v)) => Err(Fail::new(format!("{note}; {v}"))),
        (Err(e), Err(v)) => Err(Fail::new(format!("{e}; {v}"))),
    }
}

/// Add the answer `stop` of the prompt to stop the music to `result`. An error of the prompt is
/// a failure.
fn with_stop(result: Check, stop: Result<Answer, Fail>) -> Check {
    let note = match stop {
        Ok(Answer::Done) => "the owner stopped the music",
        Ok(Answer::Skip) => "the owner said that the music still plays",
        Ok(_) => "the owner did not say that the music stopped",
        Err(e) => {
            return Err(Fail::new(match result {
                Ok(_) => format!("the prompt to stop the music failed: {e}"),
                Err(body) => format!("{body}; the prompt to stop the music failed: {e}"),
            }));
        }
    };
    match result {
        Ok(mut msg) => {
            msg.push_str(", ");
            msg.push_str(note);
            Ok(msg)
        }
        Err(mut e) => {
            e.msg.push_str("; ");
            e.msg.push_str(note);
            Err(e)
        }
    }
}

/// Lower the volume with [`cap_volume`] while Bluetooth plays: the phone can set the volume. A
/// volume above [`MAX_VOLUME`](super::MAX_VOLUME) is a failure, also after it is lowered.
async fn bt_volume_ok(ctx: &mut Ctx) -> Result<(), Fail> {
    match cap_volume(ctx).await {
        Ok(None) => Ok(()),
        Ok(Some(text)) => Err(Fail::new(format!("on Bluetooth {text}"))),
        Err(e) => Err(Fail::new(format!("on Bluetooth {e}"))),
    }
}

/// Why the owner gave no answer that the check can use.
fn not_answered(answer: Answer, terminal: bool) -> &'static str {
    match answer {
        Answer::Skip if !terminal => "no terminal",
        Answer::Skip => "skipped by the owner",
        Answer::NoInput => "the input ended",
        Answer::Done | Answer::Yes | Answer::No => "no answer",
    }
}

/// The second connection says that the main zone plays the `BT` source.
fn receiver_on_bt(ctx: &Ctx) -> bool {
    ctx.oracle
        .as_ref()
        .is_some_and(|oracle| oracle.state().zone(Zone::Main).source.as_deref() == Some("BT"))
}

/// Wait for the Bluetooth source, show the media info to the owner and ask if it is right. When
/// only the second connection says `BT`, the driver must show another source than the start
/// source `old`, unless the receiver was on `BT` at the start (`start_bt`). Fails if the receiver
/// changed to `BT` but the driver still shows `old`. Before the question, a receiver volume above
/// `start`, the volume before the play prompt, is set back and the part fails.
async fn bluetooth_shown(ctx: &mut Ctx, old: &str, start_bt: bool, start: f64) -> Check {
    let mp = ctx.mp();
    let deadline = Instant::now() + Duration::from_secs(MEDIA_SECS);
    loop {
        let label = ctx.client.text(&mp, "source");
        let bt_code = receiver_on_bt(ctx);
        if is_bluetooth(&label, bt_code && (label != old || start_bt)) {
            break;
        }
        if Instant::now() >= deadline {
            if bt_code {
                let msg = format!(
                    "the receiver is on BT, the driver shows {label:?} after {MEDIA_SECS} s"
                );
                return Err(match bt_volume_ok(ctx).await {
                    Ok(()) => Fail::new(msg),
                    Err(e) => Fail::new(format!("{msg}; {e}")),
                });
            }
            return Err(Fail::skip(format!(
                "the source is not Bluetooth after {MEDIA_SECS} s: {label:?}"
            )));
        }
        ctx.client.listen_ms(200).await?;
    }
    bt_volume_ok(ctx).await?;
    // No rise above the start volume while the owner reads the question.
    bt_volume_back(ctx, start).await?;
    // The media info can come a moment after the source.
    ctx.client.listen(2).await?;
    let shown = media_shown(|attr| ctx.client.text(&mp, attr));
    let hint = format!("The media player shows: {shown}");
    let answer = ask_hint(
        ctx,
        Ask::Question("it is right", "it is wrong", SKIP_BLUETOOTH),
        "Does the player show what the phone plays?",
        Some(&hint),
    )
    .await?;
    match answer {
        Answer::Yes => Ok(format!("shows {shown}")),
        Answer::No => Err(Fail::new(format!("the player shows {shown}"))),
        other => Err(Fail::skip(format!(
            "{}, the player shows {shown}",
            not_answered(other, true)
        ))),
    }
}

/// The source is Bluetooth: the second connection says `BT` (`bt_code`), or the driver shows a
/// Bluetooth name.
fn is_bluetooth(label: &str, bt_code: bool) -> bool {
    bt_code
        || label.eq_ignore_ascii_case("BT")
        || label.ends_with(" (BT)")
        || label
            .as_bytes()
            .windows(9)
            .any(|w| w.eq_ignore_ascii_case(b"bluetooth"))
}

/// The media info that the media player shows, for the owner and the report.
fn media_shown(text: impl Fn(&str) -> String) -> String {
    const ATTRS: [(&str, &str); 6] = [
        ("source", "source"),
        ("title", "media_title"),
        ("artist", "media_artist"),
        ("album", "media_album"),
        ("image URL", "media_image_url"),
        ("state", "state"),
    ];
    let mut out = String::new();
    for (name, attr) in ATTRS {
        if !out.is_empty() {
            out.push_str(", ");
        }
        let _ = write!(out, "{name} {:?}", text(attr));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::options;
    use denon_marantz::proto::SourceInfo;

    fn source(code: &str, name: &str) -> SourceInfo {
        SourceInfo {
            code: code.to_owned(),
            name: Some(name.to_owned()),
            in_use: Some(true),
            default_name: Some(code.to_owned()),
            auto_name: None,
            hdmi_input: None,
            digital_input: None,
            analog_input: None,
            video_input: None,
        }
    }

    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn outcome(result: Check) -> (bool, bool, String) {
        match result {
            Ok(note) => (true, false, note),
            Err(f) => (false, f.skip, f.msg),
        }
    }

    #[test]
    fn parts_are_combined() {
        let pass = || Part::Pass("ok".to_owned());
        let skip = || Part::Skip("why".to_owned());
        let fail = || Part::Fail("bad".to_owned());
        assert_eq!(
            outcome(combine(&[("a", pass()), ("b", skip())])),
            (true, false, "a: ok; b skipped: why".to_owned())
        );
        assert_eq!(
            outcome(combine(&[("a", skip()), ("b", skip())])),
            (false, true, "a skipped: why; b skipped: why".to_owned())
        );
        assert_eq!(
            outcome(combine(&[("a", skip()), ("b", fail())])),
            (false, false, "a skipped: why; b failed: bad".to_owned())
        );
        assert_eq!(
            outcome(combine(&[("a", fail()), ("b", pass())])),
            (false, false, "a failed: bad; b: ok".to_owned())
        );
    }

    #[test]
    fn part_from_check() {
        assert_eq!(Part::from(Ok("n".to_owned())), Part::Pass("n".to_owned()));
        assert_eq!(Part::from(Err(Fail::skip("s"))), Part::Skip("s".to_owned()));
        assert_eq!(Part::from(Err(Fail::new("f"))), Part::Fail("f".to_owned()));
    }

    #[test]
    fn tuner_source_follows_the_source_rule() {
        let opt = options(&["oppo"]);
        let sources = [
            source("MPLAY", "Apple TV"),
            source("TUNER", "Tuner"),
            source("BD", "OPPO"),
        ];
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: true,
        };
        let names = list(&["Apple TV", "Tuner", "OPPO"]);
        assert_eq!(
            tuner_source(&rule, "Apple TV", &names),
            Ok("Tuner".to_owned())
        );
        assert_eq!(tuner_source(&rule, "Tuner", &names), Ok("Tuner".to_owned()));
        assert!(
            tuner_source(&rule, "OPPO", &names).is_err(),
            "start source avoided"
        );
        assert!(tuner_source(&rule, "", &names).is_err(), "no start source");
        let with_source = list(&["Source", "Apple TV", "Tuner"]);
        assert!(tuner_source(&rule, "Source", &with_source).is_err());
        let no_tuner = SourceRule {
            tuner: false,
            ..rule
        };
        assert_eq!(
            tuner_source(&no_tuner, "Apple TV", &names),
            Err("no tuner source may be selected".to_owned())
        );
        let avoided = options(&["oppo", "tuner"]);
        let avoid_rule = SourceRule {
            opt: &avoided,
            sources: &sources,
            tuner: true,
        };
        assert!(tuner_source(&avoid_rule, "Apple TV", &names).is_err());
        let without = [source("MPLAY", "Apple TV")];
        let rule = SourceRule {
            opt: &opt,
            sources: &without,
            tuner: true,
        };
        assert_eq!(
            tuner_source(&rule, "Apple TV", &list(&["Apple TV"])),
            Err("the receiver has no tuner source".to_owned())
        );
    }

    #[test]
    fn tuner_source_prefers_tuner_and_needs_a_listed_name() {
        let opt = options(&["oppo"]);
        let sources = [
            source("MPLAY", "Apple TV"),
            source("HDRADIO", "HD Radio"),
            source("TUNER", "Tuner"),
        ];
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: true,
        };
        assert_eq!(
            tuner_source(&rule, "Apple TV", &list(&["Apple TV", "HD Radio", "Tuner"])),
            Ok("Tuner".to_owned())
        );
        assert_eq!(
            tuner_source(&rule, "Apple TV", &list(&["Apple TV", "HD Radio"])),
            Ok("HD Radio".to_owned())
        );
        assert_eq!(
            tuner_source(&rule, "Apple TV", &list(&["Apple TV"])),
            Err("no tuner source that may be selected is in the source list".to_owned())
        );
    }

    #[test]
    fn tuner_text_follows_the_driver_rule() {
        assert_eq!(
            tuner_expected(None, None, "98.10 MHz", "FM"),
            ("98.10 MHz", "FM")
        );
        assert_eq!(
            tuner_expected(Some("Station"), Some("Artist"), "98.10 MHz", "FM"),
            ("Station", "Artist")
        );
        assert_eq!(tuner_expected(None, None, EMPTY, EMPTY), ("", ""));
        assert_eq!(tuner_expected(None, None, "", ""), ("", ""));
    }

    #[test]
    fn tuner_mismatch_reports_what_is_wrong() {
        assert_eq!(tuner_mismatch("98.10 MHz", "FM", ("98.10 MHz", "FM")), None);
        assert!(tuner_mismatch("Tuner", "FM", ("98.10 MHz", "FM")).is_some());
        assert!(tuner_mismatch("98.10 MHz", "", ("98.10 MHz", "FM")).is_some());
        assert!(tuner_mismatch("", "", ("", "")).is_some(), "no frequency");
        assert_eq!(tuner_mismatch("522 kHz", "", ("522 kHz", "")), None);
    }

    #[test]
    fn bluetooth_is_found_by_code_or_name() {
        assert!(is_bluetooth("Phone", true));
        assert!(is_bluetooth("Bluetooth", false));
        assert!(is_bluetooth("My bluetooth speaker", false));
        assert!(is_bluetooth("bt", false));
        assert!(is_bluetooth("Phone (BT)", false));
        assert!(!is_bluetooth("Tuner", false));
        assert!(!is_bluetooth("", false));
        assert!(!is_bluetooth("Blue", false));
    }

    #[test]
    fn media_is_shown_with_names() {
        let shown = media_shown(|attr| match attr {
            "media_title" => "Song".to_owned(),
            "state" => "PLAYING".to_owned(),
            _ => String::new(),
        });
        assert_eq!(
            shown,
            r#"source "", title "Song", artist "", album "", image URL "", state "PLAYING""#
        );
    }

    #[test]
    fn not_answered_says_why() {
        assert_eq!(not_answered(Answer::Skip, false), "no terminal");
        assert_eq!(not_answered(Answer::Skip, true), "skipped by the owner");
        assert_eq!(not_answered(Answer::NoInput, true), "the input ended");
    }

    #[test]
    fn stop_answer_is_added() {
        assert_eq!(
            outcome(with_stop(Ok("a".to_owned()), Ok(Answer::Done))),
            (true, false, "a, the owner stopped the music".to_owned())
        );
        assert_eq!(
            outcome(with_stop(Err(Fail::skip("a")), Ok(Answer::Skip))),
            (
                false,
                true,
                "a; the owner said that the music still plays".to_owned()
            )
        );
        assert_eq!(
            outcome(with_stop(Ok("a".to_owned()), Ok(Answer::NoInput))),
            (
                true,
                false,
                "a, the owner did not say that the music stopped".to_owned()
            )
        );
        assert_eq!(
            outcome(with_stop(Ok("a".to_owned()), Err(Fail::new("b")))),
            (
                false,
                false,
                "the prompt to stop the music failed: b".to_owned()
            )
        );
        assert_eq!(
            outcome(with_stop(Err(Fail::skip("a")), Err(Fail::new("b")))),
            (
                false,
                false,
                "a; the prompt to stop the music failed: b".to_owned()
            )
        );
    }

    #[test]
    fn volume_error_is_a_failure() {
        assert_eq!(
            outcome(with_volume(Ok("a".to_owned()), Ok(()))),
            (true, false, "a".to_owned())
        );
        assert_eq!(
            outcome(with_volume(Err(Fail::skip("a")), Ok(()))),
            (false, true, "a".to_owned())
        );
        assert_eq!(
            outcome(with_volume(Ok("a".to_owned()), Err(Fail::new("v")))),
            (false, false, "a; v".to_owned())
        );
        assert_eq!(
            outcome(with_volume(Err(Fail::skip("a")), Err(Fail::new("v")))),
            (false, false, "a; v".to_owned()),
            "a skip with a volume error is a failure"
        );
    }
}
