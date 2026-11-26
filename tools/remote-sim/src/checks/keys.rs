// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The navigation and playback keys of the media player.

use super::{
    HEOS_SOURCE, add_back_note, ensure_on, heos_now, heos_playing, join_errors, main_source,
    too_loud,
};
use crate::oracle::Oracle;
use crate::{Check, Ctx, Fail};
use denon_marantz::proto::TunerFrequency;
use denon_marantz::proto::source::is_tuner_source;
use denon_marantz::{Ask, Command, Query, Zone};
use denon_marantz_heos::{
    MediaType, NowPlaying, PlayState, PlayerId, PlayerState, QueueId, RepeatMode,
};
use serde_json::json;
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

/// The keys that are sent while the setup menu is open, in this order. The menu opens at its top
/// level, where each item opens a list and no item is a setting. Down and up only move between
/// these items. Enter opens its list and back closes that list again. Right is sent at the top
/// level and comes last: it can open a list, and in a list right can change a value, so after it no
/// other key is sent in the menu. Left is not sent: at the top level it closes the menu.
const MENU_KEYS: [&str; 5] = [
    "cursor_down",
    "cursor_up",
    "cursor_enter",
    "back",
    "cursor_right",
];

/// Time for the receiver to show the result of a key, in milliseconds.
const KEY_GAP_MS: u64 = 500;

/// Longest wait for the answer to the menu query. The receiver answers late while other queries
/// of the connection wait before them, for example after a power on.
const MENU_ANSWER: Duration = Duration::from_secs(10);

/// Next is sent only if the start track has at least this many seconds left: a track that ends
/// during the part moves the queue on.
const MIN_TIME_LEFT: i64 = 30;

/// Seconds to wait until the setup menu is closed.
const MENU_SECS: u64 = 5;

/// Seconds to wait until the setup menu opens the first time: after a power on, the receiver can
/// need more time.
const MENU_OPEN_SECS: u64 = 15;

/// Empty value of a sensor of the driver.
const EMPTY: &str = "--";

/// Seconds to wait for another track or preset after a key, and for the first HEOS position.
const CHANGE_SECS: u64 = 10;

/// Check the navigation and playback keys of the media player. Each key must get the answer OK.
/// The setup menu is closed again, also after a failure. Next and previous come back to the start
/// track, and channel up and down come back to the start preset; if that fails, the failure says
/// that the track or the preset is not set back. The keys never change the volume or the source
/// and never start playback. The step fails if a part fails, and skips only if all parts skip.
pub async fn media_keys(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let menu = Part::from(menu_part(ctx).await);
    let info = Part::from(info_part(ctx).await);
    let channel = Part::from(channel_part(ctx).await);
    let track = Part::from(track_part(ctx).await);
    combine(&[
        ("setup menu", menu),
        ("info and option", info),
        ("channel", channel),
        ("next and previous", track),
    ])
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

/// Send a media player command without parameters.
async fn press(ctx: &mut Ctx, cmd: &str) -> Result<(), Fail> {
    let mp = ctx.mp();
    ctx.client
        .command_ok("media_player", &mp, cmd, json!({}))
        .await
}

/// The second connection, if it can ask whether the setup menu is open. Else why not.
fn menu_oracle(ctx: &Ctx) -> Result<&Oracle, Fail> {
    let oracle = ctx
        .oracle
        .as_ref()
        .ok_or_else(|| Fail::skip("no second connection: the menu state is not known"))?;
    if oracle.avr().info().offers(Query::Menu) {
        Ok(oracle)
    } else {
        Err(Fail::skip("the receiver has no query for the setup menu"))
    }
}

/// A menu line of the receiver: `MNMEN ON` or `MNMEN OFF`.
fn is_menu_line(code: &str, parameter: &str) -> bool {
    code == "MN" && parameter.starts_with("MEN")
}

/// Ask the receiver on the second connection if the setup menu is open, and wait for the answer.
/// A query clears the filter of repeated events in the library, so the answer always comes as an
/// event.
async fn menu_open(oracle: &Oracle) -> Result<bool, Fail> {
    let avr = oracle.avr();
    let mut events = avr.telnet_events();
    avr.telnet_ask(&[Ask::Query(Query::Menu)]).await?;
    let end = tokio::time::Instant::now() + MENU_ANSWER;
    loop {
        match tokio::time::timeout_at(end, events.recv()).await {
            Ok(Ok(event)) if is_menu_line(event.event.code(), &event.parameter) => break,
            Ok(Ok(_) | Err(RecvError::Lagged(_))) => {}
            Ok(Err(RecvError::Closed)) | Err(_) => {
                return Err(Fail::new("the receiver did not answer the menu query"));
            }
        }
    }
    oracle
        .state()
        .device()
        .settings_menu
        .ok_or_else(|| Fail::new("the menu answer was not read"))
}

/// Wait at most `secs` until the setup menu is open (`open`) or closed.
async fn wait_menu(ctx: &mut Ctx, open: bool, secs: u64) -> Result<(), Fail> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if menu_open(menu_oracle(ctx)?).await? == open {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let state = if open { "open" } else { "closed" };
            return Err(Fail::new(format!(
                "the setup menu is not {state} after at least {secs} s"
            )));
        }
        ctx.client.listen_ms(300).await?;
    }
}

/// Open the setup menu with `menu`, send [`MENU_KEYS`] and close it with `menu`. The menu is
/// closed at the end, also after a failure. Skips if the menu is open at the start: the owner
/// can use it.
async fn menu_part(ctx: &mut Ctx) -> Check {
    if menu_open(menu_oracle(ctx)?).await? {
        return Err(Fail::skip("the setup menu is open at the start"));
    }
    let result = menu_keys(ctx).await;
    let back = menu_closed(ctx).await;
    add_back_note(result, back)
}

/// The body of [`menu_part`]. Each key of [`MENU_KEYS`] is sent only while the menu is open, so
/// that no key goes to the source.
async fn menu_keys(ctx: &mut Ctx) -> Check {
    press(ctx, "menu").await?;
    wait_menu(ctx, true, MENU_OPEN_SECS).await?;
    // The driver reads the menu state from its own connection.
    ctx.client.listen_ms(KEY_GAP_MS).await?;
    for key in MENU_KEYS {
        if !menu_open(menu_oracle(ctx)?).await? {
            return Err(Fail::new(format!(
                "the setup menu closed before {key}: {key} and the keys after it not sent"
            )));
        }
        press(ctx, key).await?;
        ctx.client.listen_ms(KEY_GAP_MS).await?;
    }
    // A second menu while the menu is closed would open it again.
    if !menu_open(menu_oracle(ctx)?).await? {
        return Err(Fail::new("the setup menu closed after cursor_right"));
    }
    // The driver closes the menu that its state shows open.
    press(ctx, "menu").await?;
    wait_menu(ctx, false, MENU_SECS).await.map_err(|e| {
        Fail::new(format!(
            "after the second menu: {e} (the driver may not know that the menu is open)"
        ))
    })?;
    Ok(format!("menu, {}, menu", MENU_KEYS.join(", ")))
}

/// Close the setup menu on the second connection if it is open or its state is not known.
async fn menu_closed(ctx: &mut Ctx) -> Result<&'static str, Fail> {
    // MNMEN OFF is also sent if the query fails: it does nothing while the menu is closed.
    if matches!(menu_open(menu_oracle(ctx)?).await, Ok(false)) {
        return Ok("the setup menu is closed");
    }
    menu_oracle(ctx)?
        .avr()
        .telnet_send(Zone::Main, Command::SetupMenu(false))
        .await?;
    wait_menu(ctx, false, MENU_SECS).await?;
    Ok("the setup menu was closed with MNMEN OFF")
}

/// Send `info`, then `context_menu`, then `back` to close the option menu. There is no query for
/// the option menu, so only the answers of the driver are checked. `back` is sent also after a
/// failed `context_menu`. Runs only while the setup menu is closed, so it needs the second
/// connection: in the setup menu, back goes up one level.
async fn info_part(ctx: &mut Ctx) -> Check {
    if menu_open(menu_oracle(ctx)?).await? {
        return Err(Fail::skip("the setup menu is open"));
    }
    press(ctx, "info").await?;
    ctx.client.listen_ms(KEY_GAP_MS * 2).await?;
    let option = press(ctx, "context_menu").await;
    ctx.client.listen_ms(KEY_GAP_MS * 2).await?;
    let back = press(ctx, "back").await;
    join_errors(option, back)?;
    if menu_open(menu_oracle(ctx)?).await? {
        return Err(Fail::new(
            "the setup menu is open after info, context_menu and back",
        ));
    }
    Ok("info, context_menu, back".to_owned())
}

/// The tuner preset and frequency of the second connection.
fn tuner_now(oracle: &Oracle) -> (Option<u8>, Option<TunerFrequency>) {
    let state = oracle.state();
    let tuner = &state.device().tuner;
    (tuner.preset, tuner.frequency)
}

/// Why channel up and down are not sent, or `None` if they may be: the driver must show the tuner
/// entities (`tuner`), the source must be the analog tuner (`code`) and the tuner must be on a
/// preset (`preset`). Else channel down does not come back to the start frequency. The driver
/// sends the presets of the analog tuner, so on the HD Radio source they could select the analog
/// tuner.
fn channel_skip(tuner: bool, code: Option<&str>, preset: Option<u8>) -> Option<String> {
    if !tuner {
        return Some("the driver shows no tuner entities".to_owned());
    }
    let Some(code) = code else {
        return Some("the source is not known".to_owned());
    };
    if !is_tuner_source(code) {
        return Some(format!("the source {code} is not the analog tuner"));
    }
    preset
        .is_none()
        .then(|| "the tuner is not on a preset: channel down would not come back".to_owned())
}

/// Send `channel_up`, then `channel_down`, and check that the tuner preset changes and comes back.
/// Runs only while the tuner is the source: a tuner command can select the tuner.
async fn channel_part(ctx: &mut Ctx) -> Check {
    let Some(oracle) = ctx.oracle.as_ref() else {
        return Err(Fail::skip(
            "no second connection: the tuner preset is not known",
        ));
    };
    let (preset, frequency) = tuner_now(oracle);
    if let Some(why) = channel_skip(ctx.tuner, main_source(ctx).as_deref(), preset) {
        return Err(Fail::skip(why));
    }
    if let Some(why) = too_loud(ctx).await? {
        return Err(Fail::skip(why));
    }
    let sensor = ctx.sensor("tuner_frequency");
    let shown = ctx.client.text(&sensor, "value");
    press(ctx, "channel_up").await.map_err(|e| {
        Fail::new(format!(
            "channel_up: {}, channel_down not sent, the preset may not be set back",
            e.msg
        ))
    })?;
    // Channel down comes back only from another preset.
    let (up, _) = wait_tuner(ctx, |p, _| p.is_some() && p != preset)
        .await
        .map_err(|e| {
            Fail::new(format!(
                "after channel_up: {e}, channel_down not sent, the preset may not be set back"
            ))
        })?;
    press(ctx, "channel_down").await.map_err(|e| {
        Fail::new(format!(
            "{e}, the preset {} is not set back to {}",
            show(up),
            show(preset)
        ))
    })?;
    // A frequency that was not known at the start is not compared.
    wait_tuner(ctx, |p, f| {
        p == preset && (frequency.is_none() || f == frequency)
    })
    .await
    .map_err(|e| {
        Fail::new(format!(
            "after channel_down: {e}, start was preset {} at {}, not set back",
            show(preset),
            show_frequency(frequency)
        ))
    })?;
    // The driver shows the frequency only with telnet.
    if !shown.is_empty() && shown != EMPTY {
        ctx.client.wait_text(&sensor, "value", 8, &shown).await?;
    }
    Ok(format!(
        "preset {} -> {} -> {}",
        show(preset),
        show(up),
        show(preset)
    ))
}

/// A preset number for the note.
fn show(preset: Option<u8>) -> String {
    preset.map_or_else(|| "none".to_owned(), |p| p.to_string())
}

/// A frequency for the note.
fn show_frequency(frequency: Option<TunerFrequency>) -> String {
    frequency.map_or_else(|| "no frequency".to_owned(), |f| f.to_string())
}

/// The second connection. The caller checked that it is there.
fn tuner_oracle(ctx: &Ctx) -> Result<&Oracle, Fail> {
    ctx.oracle
        .as_ref()
        .ok_or_else(|| Fail::new("no second connection"))
}

/// Wait until `accept` takes the tuner preset and frequency of the second connection. Returns
/// them.
async fn wait_tuner(
    ctx: &mut Ctx,
    accept: impl Fn(Option<u8>, Option<TunerFrequency>) -> bool,
) -> Result<(Option<u8>, Option<TunerFrequency>), Fail> {
    let deadline = Instant::now() + Duration::from_secs(CHANGE_SECS);
    loop {
        let (preset, frequency) = tuner_now(tuner_oracle(ctx)?);
        if accept(preset, frequency) {
            return Ok((preset, frequency));
        }
        if Instant::now() >= deadline {
            return Err(Fail::new(format!(
                "preset {} at {} after {CHANGE_SECS} s",
                show(preset),
                show_frequency(frequency)
            )));
        }
        ctx.client.listen_ms(200).await?;
    }
}

/// The title that the driver shows for a HEOS item: the song, else the station.
fn heos_title<'a>(media: &'a NowPlaying<'_>) -> &'a str {
    let blank = |t: &str| t.trim().is_empty();
    if blank(&media.song) {
        media.station.as_deref().filter(|t| !blank(t)).unwrap_or("")
    } else {
        &media.song
    }
}

/// `a` and `b` are the same queue item. An item without a queue id is not the same as any item.
fn same_track(a: &NowPlaying<'_>, b: &NowPlaying<'_>) -> bool {
    a.qid.is_some() && a.qid == b.qid
}

/// Why next and previous are not sent for the HEOS player `player` that plays `media`, or `None`
/// if they may be. Only a track of the queue comes back with previous. With shuffle on or not
/// known, previous can go to another item than the start item.
fn track_skip(player: &PlayerState, media: &NowPlaying<'_>) -> Option<&'static str> {
    let controls = player.controls();
    if media.media_type != MediaType::Song || media.qid.is_none() {
        Some("the HEOS item is not a track of the queue")
    } else if heos_title(media).is_empty() {
        Some("the HEOS item has no title")
    } else if !controls.next || !controls.previous {
        Some("the HEOS item has no next or no previous")
    } else if player.shuffle != Some(false) {
        Some("HEOS shuffle is on or not known: previous may not come back to the start item")
    } else {
        None
    }
}

/// Why next is not sent at the end of the queue, or `None` if it may be. `queue` is the number of
/// items and the last item, `qid` is the start item. Next after the last item stops playback
/// unless repeat (`repeat`) is on for all items. In a queue of one item, next does not change the
/// item.
fn queue_end_skip(
    qid: Option<QueueId>,
    queue: Option<(u32, QueueId)>,
    repeat: Option<RepeatMode>,
) -> Option<&'static str> {
    let Some((count, last)) = queue else {
        return Some("the end of the HEOS queue is not known");
    };
    if count < 2 {
        Some("the HEOS queue has only one item")
    } else if Some(last) == qid && repeat != Some(RepeatMode::OnAll) {
        Some("the HEOS item is the last of the queue: next would stop playback")
    } else {
        None
    }
}

/// The number of items and the queue id of the last item of the queue of the HEOS player `pid`.
/// Only reads. `None` if the queue is empty or cannot be read.
async fn last_queue_item(heos: &denon_marantz_heos::Heos, pid: PlayerId) -> Option<(u32, QueueId)> {
    let count = heos.queue(pid, Some((0, 0))).await.ok()?.count?;
    let index = count.checked_sub(1)?;
    let page = heos.queue(pid, Some((index, index))).await.ok()?;
    page.items.first().map(|item| (count, item.qid))
}

/// Send `next`, then `previous`, while HEOS plays on the receiver, and check that the HEOS item
/// and the title of the driver change and come back. The HEOS connection only reads. Skips if
/// the source is not the HEOS source or HEOS does not play: no playback is started.
async fn track_part(ctx: &mut Ctx) -> Check {
    use denon_marantz_heos::Heos;
    // The driver uses HEOS only on a net audio source. HEOS can also play in another zone, so
    // only the HEOS source counts.
    let source = main_source(ctx);
    if source.as_deref() != Some(HEOS_SOURCE) {
        return Err(Fail::skip(format!(
            "the source is {}, not {HEOS_SOURCE}",
            source.as_deref().unwrap_or("not known")
        )));
    }
    let heos = Heos::connect(ctx.opt.address.as_str())
        .await
        .map_err(|e| Fail::skip(format!("no HEOS: {e}")))?;
    let result = track_keys(ctx, &heos).await;
    heos.close().await;
    result
}

/// The body of [`track_part`] with the HEOS connection `heos`.
async fn track_keys(ctx: &mut Ctx, heos: &denon_marantz_heos::Heos) -> Check {
    let ip = ctx
        .opt
        .address
        .parse()
        .map_err(|_| Fail::skip("the address is not an IP address"))?;
    let pid = heos
        .player_by_ip(ip)
        .ok_or_else(|| Fail::skip("HEOS has no player with the address of the receiver"))?;
    let player = heos
        .player(pid)
        .ok_or_else(|| Fail::skip("HEOS player not read"))?;
    let Some(start) = heos_playing(&player) else {
        return Err(Fail::skip("HEOS does not play, no playback is started"));
    };
    if let Some(why) = track_skip(&player, &start) {
        return Err(Fail::skip(why));
    }
    let queue = last_queue_item(heos, pid).await;
    if let Some(why) = queue_end_skip(start.qid, queue, player.repeat) {
        return Err(Fail::skip(why));
    }
    if let Some(why) = too_loud(ctx).await? {
        return Err(Fail::skip(why));
    }
    let mp = ctx.mp();
    let start_title = heos_title(&start);
    // Without the HEOS title, the driver does not use HEOS and next goes to the source.
    if ctx
        .client
        .wait_text(&mp, "media_title", 4, start_title)
        .await
        .is_err()
    {
        return Err(Fail::skip(format!(
            "the driver shows the title {:?}, not the HEOS title {start_title:?}",
            ctx.client.text(&mp, "media_title")
        )));
    }
    // A new HEOS connection gets the position with the next progress event.
    let deadline = Instant::now() + Duration::from_secs(CHANGE_SECS);
    while heos.with_player(pid, |p| p.progress.is_none()) == Some(true) && Instant::now() < deadline
    {
        ctx.client.listen_ms(200).await?;
    }
    // HEOS is read again just before next: next on a player that does not play can start it.
    let now = heos.with_player(pid, |p| {
        let playing = p
            .current_media()
            .filter(|_| p.play_state == Some(PlayState::Play))
            .is_some_and(|m| same_track(m, &start));
        (playing, p.progress.map(|pr| pr.duration_ms), heos_now(p))
    });
    let (playing, duration_ms, position) = now.unwrap_or((false, None, None));
    if !playing {
        return Err(Fail::skip("HEOS does not play the start track any more"));
    }
    if let Some(why) = near_end(duration_ms, position) {
        return Err(Fail::skip(why));
    }
    press(ctx, "next").await.map_err(|e| {
        Fail::new(format!(
            "next: {}, previous not sent, the start item may not be set back",
            e.msg
        ))
    })?;
    let next = wait_track(ctx, heos, pid, |m| {
        m.qid.is_some() && !same_track(m, &start) && !heos_title(m).is_empty()
    })
    .await
    .map_err(|e| {
        Fail::new(format!(
            "after next: {e}, previous not sent, the start item is not set back"
        ))
    })?;
    let next_title = heos_title(&next);
    // HEOS plays the next item, so previous is safe also after a wrong driver title.
    let shown = ctx
        .client
        .wait_text(&mp, "media_title", 8, next_title)
        .await
        .map_err(|e| Fail::new(format!("after next: {e}")));
    press(ctx, "previous")
        .await
        .map_err(|e| Fail::new(format!("{e}, the start item is not set back")))?;
    wait_track(ctx, heos, pid, |m| same_track(m, &start))
        .await
        .map_err(|e| {
            Fail::new(format!(
                "after previous: {e}, the start item is not set back"
            ))
        })?;
    ctx.client
        .wait_text(&mp, "media_title", 8, start_title)
        .await
        .map_err(|e| Fail::new(format!("after previous: {e}")))?;
    shown?;
    // With the same title, the driver title does not show the change.
    let same = if next_title == start_title {
        ", same title: the driver title is not compared"
    } else {
        ""
    };
    Ok(format!(
        "{start_title:?} -> {next_title:?} -> {start_title:?}{same}"
    ))
}

/// Why next is not sent near the end of the start track, or `None` if it may be: the track of
/// `duration_ms` at `position` seconds plays less than [`MIN_TIME_LEFT`] more, or either value
/// is not known.
fn near_end(duration_ms: Option<u64>, position: Option<i64>) -> Option<&'static str> {
    let (Some(duration_ms), Some(position)) = (duration_ms, position) else {
        return Some("the position in the HEOS track is not known");
    };
    let duration = i64::try_from(duration_ms / 1000).unwrap_or(i64::MAX);
    (duration - position < MIN_TIME_LEFT).then_some("the HEOS track ends soon")
}

/// Wait until the HEOS player `pid` plays an item that `accept` takes. The caller sends no key
/// after a failure: a key while HEOS does not play could start playback.
async fn wait_track(
    ctx: &mut Ctx,
    heos: &denon_marantz_heos::Heos,
    pid: PlayerId,
    accept: impl Fn(&NowPlaying<'_>) -> bool,
) -> Result<NowPlaying<'static>, Fail> {
    let deadline = Instant::now() + Duration::from_secs(CHANGE_SECS);
    loop {
        // Only the item that `accept` takes is copied.
        let now = heos.with_player(pid, |p| {
            let media = p
                .current_media()
                .filter(|m| p.play_state == Some(PlayState::Play) && accept(m));
            (p.play_state, media.cloned())
        });
        let (state, media) = now.unwrap_or((None, None));
        if let Some(media) = media {
            return Ok(media);
        }
        if Instant::now() >= deadline {
            return Err(Fail::new(format!(
                "no expected HEOS item after {CHANGE_SECS} s, state {state:?}"
            )));
        }
        ctx.client.listen_ms(200).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn item(song: &str, station: Option<&str>, qid: Option<u64>) -> NowPlaying<'static> {
        NowPlaying {
            media_type: MediaType::Song,
            song: Cow::Owned(song.to_owned()),
            station: station.map(|s| Cow::Owned(s.to_owned())),
            album: Cow::Borrowed(""),
            artist: Cow::Borrowed(""),
            image_url: Cow::Borrowed(""),
            album_id: None,
            mid: None,
            qid: qid.map(QueueId),
            sid: None,
        }
    }

    #[test]
    fn combine_fails_skips_and_passes() {
        let pass = || Part::Pass("ok".to_owned());
        let skip = || Part::Skip("no".to_owned());
        let fail = || Part::Fail("bad".to_owned());
        assert_eq!(
            Part::from(combine(&[("a", pass()), ("b", skip())])),
            Part::Pass("a: ok; b skipped: no".to_owned())
        );
        assert_eq!(
            Part::from(combine(&[("a", skip()), ("b", skip())])),
            Part::Skip("a skipped: no; b skipped: no".to_owned())
        );
        assert_eq!(
            Part::from(combine(&[("a", skip()), ("b", fail())])),
            Part::Fail("a skipped: no; b failed: bad".to_owned())
        );
    }

    #[test]
    fn menu_keys_end_with_right() {
        // Back must come right after enter, the keys must end with right, and left closes the
        // menu, so it is not sent.
        let pos = |k: &str| MENU_KEYS.iter().position(|m| *m == k).unwrap();
        assert!(pos("cursor_enter") < pos("cursor_right"));
        assert_eq!(MENU_KEYS.last(), Some(&"cursor_right"));
        assert!(!MENU_KEYS.contains(&"cursor_left"));
        assert_eq!(pos("back"), pos("cursor_enter") + 1);
    }

    #[test]
    fn menu_line() {
        assert!(is_menu_line("MN", "MEN ON"));
        assert!(is_menu_line("MN", "MEN OFF"));
        assert!(!is_menu_line("MN", "ZST ON"));
        assert!(!is_menu_line("MV", "MEN ON"));
    }

    #[test]
    fn channel_rules() {
        assert!(channel_skip(false, Some("TUNER"), Some(1)).is_some());
        assert!(channel_skip(true, None, Some(1)).is_some());
        assert!(channel_skip(true, Some("NET"), Some(1)).is_some());
        assert!(channel_skip(true, Some("TUNER"), None).is_some());
        assert_eq!(channel_skip(true, Some("TUNER"), Some(3)), None);
        // The driver sends the presets of the analog tuner.
        assert!(channel_skip(true, Some("HDRADIO"), Some(3)).is_some());
    }

    #[test]
    fn title_is_song_else_station() {
        assert_eq!(heos_title(&item("Song", Some("Radio"), None)), "Song");
        assert_eq!(heos_title(&item(" ", Some("Radio"), None)), "Radio");
        assert_eq!(heos_title(&item("", Some(" "), None)), "");
        assert_eq!(heos_title(&item("", None, None)), "");
    }

    #[test]
    fn track_identity() {
        // The queue id decides: the same song can be in the queue twice.
        let same = |a, b| same_track(&item("A", None, a), &item("B", None, b));
        assert!(same(Some(1), Some(1)));
        assert!(!same(Some(1), Some(2)));
        assert!(!same(None, None));
        assert!(!same(None, Some(1)));
    }

    #[test]
    fn queue_end() {
        let q = |n| Some(QueueId(n));
        let last = |n| Some((5, QueueId(n)));
        assert_eq!(queue_end_skip(q(3), last(5), Some(RepeatMode::Off)), None);
        assert!(queue_end_skip(q(5), last(5), Some(RepeatMode::Off)).is_some());
        assert!(queue_end_skip(q(5), last(5), None).is_some());
        assert!(queue_end_skip(q(5), last(5), Some(RepeatMode::OnOne)).is_some());
        assert!(queue_end_skip(q(3), None, Some(RepeatMode::Off)).is_some());
        assert!(queue_end_skip(q(3), None, Some(RepeatMode::OnAll)).is_some());
        assert_eq!(queue_end_skip(q(5), last(5), Some(RepeatMode::OnAll)), None);
        // One item: next does not change it, also with repeat on.
        let one = Some((1, QueueId(1)));
        assert!(queue_end_skip(q(1), one, Some(RepeatMode::OnAll)).is_some());
    }

    #[test]
    fn track_end() {
        assert_eq!(near_end(Some(200_000), Some(100)), None);
        assert_eq!(near_end(Some(200_000), Some(170)), None);
        assert!(near_end(Some(200_000), Some(171)).is_some());
        assert!(near_end(Some(200_000), Some(300)).is_some());
        assert!(near_end(None, Some(10)).is_some());
        assert!(near_end(Some(200_000), None).is_some());
    }

    #[test]
    fn preset_note() {
        assert_eq!(show(Some(4)), "4");
        assert_eq!(show(None), "none");
    }
}
