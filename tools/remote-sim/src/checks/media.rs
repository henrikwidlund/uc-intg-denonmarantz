// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The media info of the tuner and of Bluetooth.

use super::parts::{EMPTY, Part, combine};
use super::prompt::{
    Answer, Ask, Raised, Watched, add_failure, ask_watched, end_watch, listen_watched, note_rise,
    owner_ask_watched, rise_back, rise_result, stop_fail, stops, terminal, watched,
};
use super::{
    SourceRule, VOLUME_NOT_SAFE, add_back_note, add_set_back, cap_volume, driver_volume, ensure_on,
    join_errors, join_rises, label_fits, owner_back, owner_result, owner_source_back, read_sources,
    receiver_volume, restore_source, source_back, source_rule, start_source_ok, start_volume,
    strings,
};
use crate::{Check, Ctx, Fail};
use denon_marantz::Zone;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// What s + Enter means at the play prompt and at the question of the Bluetooth part.
const SKIP_BLUETOOTH: &str = "skip the Bluetooth part";

/// What s + Enter means at the prompt to stop the music. It only adds a note.
const MUSIC_PLAYS: &str = "the music still plays";

/// Why a part that makes sound does not run.
const NO_SOUND: &str = "nothing that makes sound runs";

/// Why the Bluetooth part does not run after the tuner part.
const NO_START_SOURCE: &str = "the tuner part did not set the start source back, or a rise of the \
                               receiver volume in it was not lowered or a read failed";

/// How often the prompt to stop the music is asked at most, when a rise comes while or right after
/// it.
const STOP_ASKS: usize = 3;

/// Seconds to wait for the media info or the Bluetooth source.
const MEDIA_SECS: u64 = 10;

/// Check the media info of the media player: the tuner shows the text of the driver rule (the title
/// of the receiver, else the frequency; the artist of the receiver, else the band), and the owner
/// checks what Bluetooth shows. The step fails if a part fails, and skips only if all parts skip.
/// The start source is read once, before the parts. The Bluetooth part skips if the start source is
/// not back after the tuner part, or if a rise of the receiver volume in the tuner part was not
/// lowered or a volume read failed. A rise that was lowered does not skip it by itself.
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
    let mp = ctx.mp();
    let old = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    let (tuner, tuner_back_ok) = tuner_part(ctx, &old, &list).await;
    let tuner = Part::from(tuner);
    let bluetooth = if tuner_back_ok {
        Part::from(bluetooth_part(ctx, &old, &list).await)
    } else {
        Part::Skip(NO_START_SOURCE.to_owned())
    };
    combine(&[("tuner", tuner), ("Bluetooth", bluetooth)])
}

/// The name in the driver list `list` of a tuner source that may be selected, or why there is
/// none. The start source `old` must be one that may be selected: the set back selects it again.
/// Only `TUNER`: HD radio shows no frequency when it has no title. Only the selected source must
/// pass the rule, because the check selects it by its name.
fn tuner_source(rule: &SourceRule<'_>, old: &str, list: &[String]) -> Result<String, String> {
    start_source_ok(rule, old, list)?;
    let Some(info) = rule.info("TUNER") else {
        return Err("the receiver has no TUNER source".to_owned());
    };
    if !rule.code_ok("TUNER") {
        return Err("the TUNER source may not be selected: it is in --avoid-sources, no --avoid-sources is given, it is not in use or has no name, or the driver shows no tuner".to_owned());
    }
    list.iter()
        .find(|l| label_fits(info, l) && rule.label_ok(l))
        .cloned()
        .ok_or_else(|| "no tuner source that may be selected is in the source list".to_owned())
}

/// Select a tuner source, check the media info and set the start source `old` of the driver list
/// `list` back, also after a failure. A receiver volume above the start volume from [`tuner_label`]
/// after the sources are read is set back there, and the part fails before the tuner is selected.
/// While the tuner plays, the receiver volume is watched with [`watched`] and this start volume: a
/// volume above it ends the wait, is lowered and fails. Right after the wait, on every result, the
/// watch lowers a volume above the start volume (a failure). If a rise was not lowered or a volume
/// read failed, the check selects no source: the owner is asked with [`owner_source_back`]. Such a
/// rise or failed read right after a set back that worked asks no owner: the source is back. Else
/// the check sets the source back with [`restore_source`], and if that fails, the owner is asked,
/// watched with the same start volume: a failed set back of the check is a failure also when the
/// owner sets the source back. Returns the result, and true only if the start source is back, no
/// rise stayed not lowered and the reads before the tuner plays did not fail. A rise that was
/// lowered does not make it false. False means that no more sound part may run.
async fn tuner_part(ctx: &mut Ctx, old: &str, list: &[String]) -> (Check, bool) {
    let (label, start) = match tuner_label(ctx, old, list).await {
        Ok((label, start, None)) => (label, start),
        // The part fails before the tuner plays. A rise that was lowered does not stop the next
        // part.
        Ok((_, _, Some(rise))) => return (Err(rise.fail), rise.lowered),
        // An error that is not a skip is a failed read of the volume or of the WebSocket.
        Err(e) => {
            let ok = e.skip;
            return (Err(e), ok);
        }
    };
    let selected = label != old;
    let mp = ctx.mp();
    let waited = watched(
        ctx,
        start,
        "while the tuner played",
        async |ctx: &mut Ctx| {
            if selected {
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
                    .map_err(|e| {
                        Fail::new(format!("{e}. Wanted: {label}, source list: {list:?}"))
                    })?;
            }
            tuner_shown(ctx, &label).await
        },
    )
    .await;
    // The volume goes back first: a source set back must not start at a volume that the owner set.
    let (result, rise) = end_watch(waited);
    let stop = stops(&rise);
    let volume = rise_result(rise);
    if !selected {
        let result = result.map(|note| format!("{note}, the tuner was the start source"));
        // The start source did not change.
        return (add_failure(result, volume), !stop);
    }
    if stop {
        // The check selects no source: the owner sets it back. The rise was not lowered, so no
        // more sound part runs.
        let back = owner_source_back(ctx, old, VOLUME_NOT_SAFE, start).await;
        return (add_set_back(add_failure(result, volume), back), false);
    }
    let (back, back_rise) = restore_source(ctx, old, 4, start).await;
    let (back, source_back) = match back {
        Ok(tries) => (
            Ok(format!("source set back to {old} in {tries} tries")),
            true,
        ),
        Err(e) => {
            let why = format!("the set back failed: {e}");
            let (owner, owner_rise) = owner_back(ctx, old, &why, start).await;
            let owner_ok = owner.as_ref().is_ok_and(|back| !back.skipped);
            // With the error of the check, the result is always an error.
            let fail = match owner_result(Some(e.msg), owner) {
                Err(fail) => fail,
                Ok(back) => Fail::new(back.note),
            };
            let back_ok = owner_ok && !stops(&owner_rise);
            (add_failure(Err(fail), rise_result(owner_rise)), back_ok)
        }
    };
    let back_ok = source_back && !stops(&back_rise);
    let volume = join_errors(volume, rise_result(back_rise));
    (add_failure(add_back_note(result, back), volume), back_ok)
}

/// The tuner source of the driver list `list` that [`tuner_part`] selects, the start volume from
/// [`start_volume`] and the rise that [`rise_back`] finds after the sources are read, or why the
/// part does not run. `old` is the start source. With a rise, the part fails before the tuner
/// plays.
async fn tuner_label(
    ctx: &mut Ctx,
    old: &str,
    list: &[String],
) -> Result<(String, f64, Option<Raised>), Fail> {
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
    // The start volume is read first: the start source plays while the sources are read. A skip in
    // the reads runs no `rise_back`: the check has started no sound.
    let receiver = receiver_volume(ctx).await?;
    let start = start_volume(receiver, driver_volume(ctx).await?)
        .map_err(|why| Fail::skip(format!("{why}: {NO_SOUND}")))?;
    let sources = read_sources(ctx).await.map_err(Fail::skip)?;
    let label = tuner_source(&source_rule(ctx, &sources), old, list).map_err(Fail::skip)?;
    let rise = rise_back(ctx, start, "while the check read the sources").await;
    Ok((label, start, rise))
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

/// The owner plays music from a phone over Bluetooth and says if the media player shows it. The
/// source rule does not apply to the `BT` source: the owner selects it. The start volume comes from
/// [`start_volume`]: it is the receiver volume. A receiver volume above it after the sources are
/// read is set back with [`rise_back`], and the part fails before the play prompt. The prompts and
/// the waits are watched with this start volume, see [`ask_watched`] and [`bluetooth_shown`]. At
/// the end, on every answer, the check reads the events while it watches the volume (see
/// [`listen_watched`]): a receiver volume above the start volume is set back. Then the start source
/// `old` of the driver list `list` is set back: through the owner if a rise was not lowered or a
/// volume read failed. Then the owner stops the music (see [`ask_stop`]), watched too. A skipped
/// set back of the source makes the part skip if nothing failed.
async fn bluetooth_part(ctx: &mut Ctx, old: &str, list: &[String]) -> Check {
    if !terminal() {
        return Err(Fail::skip(not_answered(Answer::NoTerminal)));
    }
    // The start volume is read first: the start source plays while the sources are read. A skip in
    // the reads runs no `rise_back`: the check has started no sound.
    let Some(receiver) = receiver_volume(ctx).await? else {
        return Err(Fail::skip(
            "no second connection: the receiver volume is not known",
        ));
    };
    let start = start_volume(Some(receiver), driver_volume(ctx).await?)
        .map_err(|why| Fail::skip(format!("{why}: {NO_SOUND}")))?;
    // The input levels must pass `levels_ok`: a higher level makes the source louder.
    read_sources(ctx).await.map_err(Fail::skip)?;
    if let Some(rise) = rise_back(ctx, start, "while the check read the sources").await {
        return Err(rise.fail);
    }
    let start_bt = receiver_on_bt(ctx);
    let answer = ask_watched(
        ctx,
        Ask::Action {
            done: "it plays",
            skip: SKIP_BLUETOOTH,
        },
        "Play music from a phone over Bluetooth to the receiver. This makes sound.",
        None,
        start,
    )
    .await;
    // The owner may have started Bluetooth also before a skip or an error. The skip and error
    // paths do not wait in `bluetooth_shown`, so the source and the volume get 2 s to change
    // before they are compared.
    let mut stop = false;
    let (body, settle_ms) = match answer {
        Ok(Watched::Done(Answer::Done)) => (
            bluetooth_shown(ctx, old, start_bt, start, &mut stop).await,
            300,
        ),
        Ok(Watched::Done(other)) => (Err(Fail::skip(not_answered(other))), 2000),
        Ok(Watched::Ended(rise) | Watched::After(_, rise)) => {
            (Err(stop_fail(&mut stop, rise)), 2000)
        }
        Err(e) => (Err(e), 2000),
    };
    // The volume goes back first: a source set back must not start at a volume that the phone set.
    let (settle, rise) = listen_watched(ctx, settle_ms, start, "after Bluetooth").await;
    stop |= stops(&rise);
    let settle = settle.map_err(|e| Fail::new(format!("the events could not be read: {e}")));
    let volume = join_errors(rise_result(rise), settle);
    // `source_back` sends nothing if the receiver has the start source.
    let back = source_back(ctx, old, list, start, stop.then_some(VOLUME_NOT_SAFE)).await;
    let (stop, ended) = ask_stop(ctx, start).await;
    let result = ended.into_iter().fold(
        add_failure(add_set_back(body, back), volume),
        |result, e| add_failure(result, Err(e)),
    );
    with_stop(result, stop)
}

/// Ask the owner to stop the music with [`owner_ask_watched`], also after a stop: the phone can set
/// the volume. A prompt with a rise while or right after it is asked again after the lowering, at
/// most [`STOP_ASKS`] times in total. Returns the result of the last prompt, and the failures of
/// the earlier prompts with a rise.
async fn ask_stop(ctx: &mut Ctx, start: f64) -> (Result<Watched, Fail>, Vec<Fail>) {
    let mut ended = Vec::new();
    loop {
        let watched = owner_ask_watched(
            ctx,
            Ask::Action {
                done: "done",
                skip: MUSIC_PLAYS,
            },
            "Stop the music on the phone.",
            None,
            start,
        )
        .await;
        match watched {
            Ok(Watched::Ended(rise) | Watched::After(_, rise)) if ended.len() + 1 < STOP_ASKS => {
                ended.push(rise.fail);
            }
            watched => return (watched, ended),
        }
    }
}

/// Add the result `stop` of the prompt to stop the music to `result`. A rise while or right after
/// the prompt (see [`Watched::Ended`] and [`Watched::After`]) and an error of the prompt are
/// failures.
fn with_stop(result: Check, stop: Result<Watched, Fail>) -> Check {
    let failed = |text: String| {
        Err(Fail::new(match &result {
            Ok(_) => text,
            Err(body) => format!("{body}; {text}"),
        }))
    };
    let note = match stop {
        Ok(Watched::Done(Answer::Done)) => "the owner stopped the music",
        Ok(Watched::Done(Answer::Skip)) => "the owner said that the music still plays",
        Ok(Watched::Done(Answer::Yes | Answer::No | Answer::NoInput | Answer::NoTerminal)) => {
            "the owner did not say that the music stopped"
        }
        Ok(Watched::Ended(rise)) => {
            return failed(format!("the stop prompt ended early: {}", rise.fail));
        }
        Ok(Watched::After(_, rise)) => {
            return failed(format!("right after the stop prompt: {}", rise.fail));
        }
        Err(e) => return failed(format!("the prompt to stop the music failed: {e}")),
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
/// volume above [`MAX_VOLUME`](super::MAX_VOLUME) is a failure, also after it is lowered. An error
/// of [`cap_volume`] gives a [`Raised`] that was not lowered: the volume can still be above the
/// limit. `watch` is the start volume for the watch of [`cap_volume`], `None` if the caller
/// watches already.
async fn bt_volume_ok(ctx: &mut Ctx, watch: Option<f64>) -> Result<(), Raised> {
    match cap_volume(ctx, watch).await {
        Ok(None) => Ok(()),
        Ok(Some(text)) => Err(note_rise(
            ctx,
            Raised::new(format!("on Bluetooth {text}"), true),
        )),
        Err(e) => Err(note_rise(
            ctx,
            Raised::new(format!("on Bluetooth {e}"), false),
        )),
    }
}

/// Why the owner gave no answer that the check can use.
fn not_answered(answer: Answer) -> &'static str {
    match answer {
        Answer::NoTerminal => "no terminal",
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
/// only the second connection says `BT`, the driver must show another source than the start source
/// `old`, unless the receiver was on `BT` at the start (`start_bt`). Fails if the receiver changed
/// to `BT` but the driver still shows `old`. The waits and the question are watched with
/// [`watched`] and [`ask_watched`]: a receiver volume above `start`, the volume before the play
/// prompt, while or right after each of them is set back and the part fails. A rise that was not
/// lowered or a failed volume read sets `stop`: no more sound prompts, and the source set back goes
/// through the owner.
async fn bluetooth_shown(
    ctx: &mut Ctx,
    old: &str,
    start_bt: bool,
    start: f64,
    stop: &mut bool,
) -> Check {
    let mp = ctx.mp();
    let waited = watched(
        ctx,
        start,
        "while the check waited for Bluetooth",
        async |ctx: &mut Ctx| wait_bluetooth(ctx, old, start_bt).await,
    )
    .await?;
    let rise = match waited {
        Watched::Done(Ok(())) => bt_volume_ok(ctx, Some(start)).await.err(),
        Watched::Done(Err(rise)) | Watched::Ended(rise) => Some(rise),
        Watched::After(Err(first), rise) => Some(join_rises(first, rise)),
        Watched::After(Ok(()), rise) => Some(rise),
    };
    if let Some(rise) = rise {
        return Err(stop_fail(stop, rise));
    }
    // The media info can come a moment after the source.
    let waited = watched(
        ctx,
        start,
        "while the check waited for the media info",
        async |ctx: &mut Ctx| ctx.client.listen(2).await,
    )
    .await?;
    // No rise above the start volume while the owner reads the question.
    if let Watched::Ended(rise) | Watched::After(_, rise) = waited {
        return Err(stop_fail(stop, rise));
    }
    let shown = media_shown(|attr| ctx.client.text(&mp, attr));
    let hint = format!("The media player shows: {shown}");
    let answer = match ask_watched(
        ctx,
        Ask::Question {
            yes: "it is right",
            no: "it is wrong",
            skip: SKIP_BLUETOOTH,
        },
        "Does the player show what the phone plays?",
        Some(&hint),
        start,
    )
    .await?
    {
        Watched::Done(answer) => answer,
        Watched::Ended(rise) | Watched::After(_, rise) => {
            return Err(stop_fail(stop, rise));
        }
    };
    match answer {
        Answer::Yes => Ok(format!("shows {shown}")),
        Answer::No => Err(Fail::new(format!("the player shows {shown}"))),
        other => Err(Fail::skip(format!(
            "{}, the player shows {shown}",
            not_answered(other)
        ))),
    }
}

/// Wait up to [`MEDIA_SECS`] for the Bluetooth source, see [`bluetooth_shown`]. Skips if the
/// source does not change. A volume that [`bt_volume_ok`] did not lower gives a [`Raised`] that
/// was not lowered. The caller watches the receiver volume while it waits.
async fn wait_bluetooth(
    ctx: &mut Ctx,
    old: &str,
    start_bt: bool,
) -> Result<Result<(), Raised>, Fail> {
    let mp = ctx.mp();
    let deadline = Instant::now() + Duration::from_secs(MEDIA_SECS);
    loop {
        let label = ctx.client.text(&mp, "source");
        let bt_code = receiver_on_bt(ctx);
        if is_bluetooth(&label, bt_code && (label != old || start_bt)) {
            return Ok(Ok(()));
        }
        if Instant::now() >= deadline {
            if bt_code {
                let msg = format!(
                    "the receiver is on BT, the driver shows {label:?} after {MEDIA_SECS} s"
                );
                return match bt_volume_ok(ctx, None).await {
                    Ok(()) => Err(Fail::new(msg)),
                    Err(rise) if rise.lowered => Err(Fail::new(format!("{msg}; {}", rise.fail))),
                    Err(rise) => Ok(Err(Raised {
                        fail: Fail::new(format!("{msg}; {}", rise.fail)),
                        lowered: false,
                    })),
                };
            }
            return Err(Fail::skip(format!(
                "the source is not Bluetooth after {MEDIA_SECS} s: {label:?}"
            )));
        }
        ctx.client.listen_ms(200).await?;
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
            Err("the TUNER source may not be selected: it is in --avoid-sources, no --avoid-sources is given, it is not in use or has no name, or the driver shows no tuner".to_owned())
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
            Err("the receiver has no TUNER source".to_owned())
        );
    }

    #[test]
    fn tuner_source_is_only_the_tuner_and_needs_a_listed_name() {
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
            Err("no tuner source that may be selected is in the source list".to_owned())
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
        assert_eq!(not_answered(Answer::NoTerminal), "no terminal");
        assert_eq!(not_answered(Answer::Skip), "skipped by the owner");
        assert_eq!(not_answered(Answer::NoInput), "the input ended");
    }

    #[test]
    fn stop_answer_is_added() {
        assert_eq!(
            outcome(with_stop(
                Ok("a".to_owned()),
                Ok(Watched::Done(Answer::Done))
            )),
            (true, false, "a, the owner stopped the music".to_owned())
        );
        assert_eq!(
            outcome(with_stop(
                Err(Fail::skip("a")),
                Ok(Watched::Done(Answer::Skip))
            )),
            (
                false,
                true,
                "a; the owner said that the music still plays".to_owned()
            )
        );
        assert_eq!(
            outcome(with_stop(
                Ok("a".to_owned()),
                Ok(Watched::Done(Answer::NoInput))
            )),
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
        let ended = |msg: &str| Watched::Ended(Raised::new(msg.to_owned(), true));
        assert_eq!(
            outcome(with_stop(Ok("a".to_owned()), Ok(ended("b")))),
            (false, false, "the stop prompt ended early: b".to_owned())
        );
        assert_eq!(
            outcome(with_stop(Err(Fail::skip("a")), Ok(ended("b")))),
            (false, false, "a; the stop prompt ended early: b".to_owned())
        );
    }

    #[test]
    fn volume_error_is_a_failure() {
        assert_eq!(
            outcome(add_failure(Ok("a".to_owned()), Ok(()))),
            (true, false, "a".to_owned())
        );
        assert_eq!(
            outcome(add_failure(Err(Fail::skip("a")), Ok(()))),
            (false, true, "a".to_owned())
        );
        assert_eq!(
            outcome(add_failure(Ok("a".to_owned()), Err(Fail::new("v")))),
            (false, false, "a; v".to_owned())
        );
        assert_eq!(
            outcome(add_failure(Err(Fail::skip("a")), Err(Fail::new("v")))),
            (false, false, "a; v".to_owned()),
            "a skip with a volume error is a failure"
        );
    }
}
