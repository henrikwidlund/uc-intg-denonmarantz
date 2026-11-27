// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The checks of the steps in `crate::STEPS`.

use crate::oracle::{self, Oracle};
use crate::restore;
use crate::ws::{Client, close_echo, field_ids, field_value};
use crate::{Check, Ctx, Fail};
use denon_marantz::proto::source::{is_playing_source, telnet_to_display_source};
use denon_marantz::proto::{DiracFilter, Power, SourceInfo, SourceLevels};
use denon_marantz::{Command, Query, Zone};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::time::sleep;

mod keys;
mod lookup;
mod media;
mod prompt;
mod signal;

pub use keys::media_keys;
pub use lookup::address_lookup;
pub use media::media_info;
use prompt::{Answer, Ask, SKIP_CHECK, ask};
pub use prompt::{stdin_lines, terminal};
pub use signal::signal;

/// The UI pages and the simple commands of the driver.
const UI_PAGES: &str = include_str!("../../../resources/remote_ui.json");
const COMMANDS: &str = include_str!("../../../resources/commands.tsv");

/// Highest volume that a check sets, on the scale of the Remote (dB plus 80): -40 dB.
pub const MAX_VOLUME: f64 = 40.0;

macro_rules! ensure {
    ($cond:expr, $($msg:tt)+) => {
        let ok: bool = $cond;
        if !ok {
            return Err(Fail::new(format!($($msg)+)));
        }
    };
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

fn is_on(state: &str) -> bool {
    matches!(state, "ON" | "PLAYING" | "PAUSED")
}

async fn entity_list(ctx: &mut Ctx) -> Result<Vec<Value>, Fail> {
    let data = ctx
        .client
        .request_ok("get_available_entities", Value::Null)
        .await?;
    Ok(data["available_entities"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

async fn entity_ids(ctx: &mut Ctx) -> Result<BTreeSet<String>, Fail> {
    Ok(entity_list(ctx)
        .await?
        .iter()
        .filter_map(|e| e["entity_id"].as_str().map(str::to_owned))
        .collect())
}

/// Subscribe to all entities like the Remote does, then read their states.
async fn subscribe_all(ctx: &mut Ctx) -> Result<usize, Fail> {
    let ids: Vec<String> = entity_ids(ctx).await?.into_iter().collect();
    ctx.client
        .request_ok("subscribe_events", json!({ "entity_ids": ids }))
        .await?;
    ctx.client.refresh_states().await?;
    Ok(ids.len())
}

/// Sets [`Ctx::tuner`] from `config.json`. The driver checks the tuner again at each connect.
async fn read_tuner(ctx: &mut Ctx) -> Result<(), Fail> {
    let config = read_config(ctx).await?;
    let device = config_device(ctx, &config)?;
    ctx.tuner = has_tuner(device);
    Ok(())
}

/// The same rule as the driver: without the setup option, only a receiver that says it has no
/// tuner gets no tuner entities.
fn has_tuner(device: &Value) -> bool {
    device["tuner"]
        .as_bool()
        .unwrap_or(device["has_tuner"] != false)
}

/// The Dirac filter that a `DIRAC_LIVE_FILTER_*` command selects.
fn dirac_filter(name: &str) -> Option<DiracFilter> {
    Some(match name.strip_prefix("DIRAC_LIVE_FILTER_")? {
        "SLOT1" => DiracFilter::Slot1,
        "SLOT2" => DiracFilter::Slot2,
        "SLOT3" => DiracFilter::Slot3,
        "OFF" => DiracFilter::Off,
        _ => return None,
    })
}

/// The second connection says that the slot of the filter has no filter. The receiver ignores
/// such a slot.
fn empty_slot(ctx: &Ctx, filter: DiracFilter) -> bool {
    let slot = match filter {
        DiracFilter::Slot1 => 0,
        DiracFilter::Slot2 => 1,
        DiracFilter::Slot3 => 2,
        _ => return false,
    };
    ctx.oracle.as_ref().is_some_and(|oracle| {
        oracle
            .state()
            .device()
            .dirac_slots
            .get(slot)
            .and_then(|s| s.imported)
            == Some(false)
    })
}

/// The Dirac filter that the second connection has. `None` without that connection.
fn oracle_dirac(ctx: &Ctx) -> Option<DiracFilter> {
    ctx.oracle.as_ref()?.state().device().dirac_filter
}

/// Wait for a state of the media player that is not `UNAVAILABLE`.
async fn wait_available(ctx: &mut Ctx, secs: u64) -> Result<String, Fail> {
    let mp = ctx.mp();
    let state = ctx
        .client
        .wait_attr(&mp, "state", secs, |v| {
            v.as_str().is_some_and(|s| s != "UNAVAILABLE")
        })
        .await?;
    Ok(state.as_str().unwrap_or_default().to_owned())
}

fn norm(text: &str) -> String {
    text.parse::<f64>()
        .map_or_else(|_| text.to_owned(), |f| format!("{f:.1}"))
}

fn shown(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => norm(s),
        Some(Value::Number(n)) => norm(&n.to_string()),
        Some(other) => other.to_string(),
        None => "none".to_owned(),
    }
}

/// Values that depend on the source: the receiver answers the query for some sources only, for
/// example not for an HDMI source. The second connection can hold the value of another source.
const NOT_REPORTED: [&str; 4] = [
    "select.digital_codec",
    "sensor.digital_codec",
    "select.picture_mode",
    "sensor.picture_mode",
];

/// Values that come from the web pages of the receiver. HDR is not listed: with SDR it has no
/// value, so it is always compared.
const WEB_PAGE_VALUES: [&str; 10] = [
    "select.monitor_output",
    "sensor.monitor_output",
    "select.picture_mode",
    "sensor.picture_mode",
    "select.digital_codec",
    "sensor.digital_codec",
    "sensor.colorspace_input",
    "sensor.colorspace_output",
    "sensor.pixel_depth_input",
    "sensor.pixel_depth_output",
];

/// Values of selects and sensors that differ from the values of the receiver.
fn mismatches(ctx: &Ctx, offered: &BTreeSet<String>) -> Vec<String> {
    let Some(oracle) = &ctx.oracle else {
        return Vec::new();
    };
    let state = oracle.state();
    let on = matches!(state.zone(Zone::Main).power, Some(Power::On));
    if !on || !is_on(&ctx.client.text(&ctx.mp(), "state")) {
        return Vec::new();
    }
    let mut bad = Vec::new();
    for (name, expected) in oracle::expected(&state) {
        let Some((kind, rest)) = name.split_once('.') else {
            continue;
        };
        let id = format!("{kind}.{rest}.{}", ctx.key);
        // The driver leaves out the entities of features that the receiver does not list.
        if !offered.contains(&id) {
            continue;
        }
        let attr = match kind {
            "select" => "current_option",
            "switch" => "state",
            _ => "value",
        };
        let got = shown(ctx.client.attr(&id, attr));
        // The web pages of the receiver sometimes answer with an error. The second connection
        // then has no value: that is not a difference of the driver.
        if (expected.is_none() && WEB_PAGE_VALUES.contains(&name)) || NOT_REPORTED.contains(&name) {
            continue;
        }
        // The driver hides the tuner entities on a receiver without a tuner.
        if !ctx.tuner && rest.starts_with("tuner_") {
            continue;
        }
        let empty = if kind == "switch" { "UNKNOWN" } else { "--" };
        let want = norm(expected.as_deref().unwrap_or(empty));
        if got != want {
            bad.push(format!("{name}: shows {got}, receiver has {want}"));
        }
    }
    bad
}

/// Wait until all entities show what the receiver has.
async fn verify_all(ctx: &mut Ctx, secs: u64) -> Result<(), Fail> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let offered = entity_ids(ctx).await?;
    loop {
        let bad = mismatches(ctx, &offered);
        if bad.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Fail::new(format!(
                "different from the receiver: {}",
                bad.join("; ")
            )));
        }
        ctx.client.listen_ms(300).await?;
    }
}

/// Read the receiver again on the second connection and compare.
async fn verify_fresh(ctx: &mut Ctx) -> Result<(), Fail> {
    if let Some(oracle) = &ctx.oracle {
        oracle.refresh().await?;
    }
    ctx.client.listen(1).await?;
    verify_all(ctx, 6).await
}

fn current_volume(ctx: &Ctx) -> Option<f64> {
    ctx.client.attr(&ctx.mp(), "volume")?.as_f64()
}

/// The higher of two volumes. A missing volume does not count.
fn highest(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The lower of two volumes. A missing volume does not count.
fn lowest(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The volume that the driver shows, after the waiting events are read. It can be old: the
/// library keeps the volume of before a standby.
async fn driver_volume(ctx: &mut Ctx) -> Result<Option<f64>, Fail> {
    ctx.client.listen_ms(300).await?;
    Ok(current_volume(ctx))
}

/// The volume that the receiver has, asked on the second connection. `None` without that
/// connection.
async fn receiver_volume(ctx: &Ctx) -> Result<Option<f64>, Fail> {
    match &ctx.oracle {
        Some(oracle) => Ok(Some(oracle.fresh_volume().await?)),
        None => Ok(None),
    }
}

/// The volume before a change, as the limit for a set back: the lower of the receiver and the
/// driver volume.
async fn baseline_volume(ctx: &mut Ctx) -> Result<Option<f64>, Fail> {
    let receiver = receiver_volume(ctx).await?;
    Ok(lowest(receiver, driver_volume(ctx).await?))
}

/// The highest known volume: the higher of the receiver and the driver volume. Fails without a
/// known volume.
async fn highest_volume(ctx: &mut Ctx) -> Result<f64, Fail> {
    let receiver = receiver_volume(ctx).await?;
    highest(receiver, driver_volume(ctx).await?).ok_or_else(|| Fail::new("no volume"))
}

/// Why a change that makes sound may not start: the higher of the receiver and the driver volume
/// is above [`MAX_VOLUME`] or not known. `None` if it may start.
async fn too_loud(ctx: &mut Ctx) -> Result<Option<String>, Fail> {
    let receiver = receiver_volume(ctx).await?;
    Ok(loud_volume(highest(receiver, driver_volume(ctx).await?)))
}

/// Lower the volume to [`MAX_VOLUME`] if the higher of the receiver and the driver volume is above
/// it: the owner or a phone can set the volume. `None` if it is not above or not known. `Some` with
/// the values if it was above: the caller fails with this text. Only a receiver volume above the
/// limit is lowered, see [`lower_volume`]. Error if it was not lowered.
async fn cap_volume(ctx: &mut Ctx) -> Result<Option<String>, Fail> {
    let receiver = receiver_volume(ctx).await?;
    let driver = driver_volume(ctx).await?;
    let Some(high) = highest(receiver, driver).filter(|v| *v > MAX_VOLUME) else {
        return Ok(None);
    };
    let known = |v: Option<f64>| v.map_or_else(|| "unknown".to_owned(), |v| v.to_string());
    let values = format!(
        "the volume {high} (receiver {}, driver {}) is above the limit {MAX_VOLUME}",
        known(receiver),
        known(driver)
    );
    match lower_volume(ctx, None).await {
        // The driver can show an old, higher volume.
        Ok(()) if receiver.is_some_and(|r| r <= MAX_VOLUME) => Ok(Some(format!(
            "{values}, nothing was sent: the receiver is not above the limit"
        ))),
        Ok(()) => Ok(Some(format!("{values}, it was lowered to {MAX_VOLUME}"))),
        Err(e) => Err(Fail::new(format!("{values}, not lowered: {e}"))),
    }
}

/// Why nothing that makes sound may start at the volume `volume`, or `None` if it may.
fn loud_volume(volume: Option<f64>) -> Option<String> {
    sound_volume(volume).err()
}

/// The volume `volume` if something that makes sound may start at it. Error: why not, the volume
/// is above [`MAX_VOLUME`] or not known.
fn sound_volume(volume: Option<f64>) -> Result<f64, String> {
    match volume {
        None => Err("the volume is not known".to_owned()),
        Some(v) if v <= MAX_VOLUME => Ok(v),
        Some(v) => Err(format!("the volume {v} is above the limit {MAX_VOLUME}")),
    }
}

/// Switch the receiver on. A power on can set the power-on volume of the receiver: it is lowered
/// at once if it is above the limit. The library keeps the volume of before the standby, so the
/// driver can show that old volume at once. The receiver is asked on the second connection, and
/// without that connection the receiver is not switched on.
async fn power_on(ctx: &mut Ctx) -> Result<String, Fail> {
    if ctx.oracle.is_none() {
        return Err(Fail::skip(
            "no second connection: no power on, the volume after it cannot be read from the receiver",
        ));
    }
    let mp = ctx.mp();
    let on = async {
        ctx.client
            .command_ok("media_player", &mp, "on", json!({}))
            .await?;
        ctx.client
            .wait_attr(&mp, "state", 20, |v| v.as_str().is_some_and(is_on))
            .await?;
        ctx.client.listen(1).await
    }
    .await;
    // The volume check runs also after an error: the receiver can be on.
    let check = power_on_volume(ctx).await;
    match on {
        Ok(()) => check,
        Err(e) => join_errors(Err(Fail::new(format!("power on: {e}"))), check.map(|_| ()))
            .map(|()| String::new()),
    }
}

/// The volume check after a power on: read the volume of the receiver, with one more try. If it
/// is not known, mute. If it is above the limit, lower it and fail.
async fn power_on_volume(ctx: &mut Ctx) -> Result<String, Fail> {
    let mp = ctx.mp();
    let ask = async |ctx: &mut Ctx| {
        receiver_volume(ctx)
            .await
            .and_then(|v| v.ok_or_else(|| Fail::new("no second connection")))
    };
    // One more try, then mute: the volume can be loud.
    let v = match ask(ctx).await {
        Ok(v) => v,
        Err(_) => match ask(ctx).await {
            Ok(v) => v,
            Err(e) => {
                let mute = async {
                    ctx.client
                        .command_ok("media_player", &mp, "mute", json!({}))
                        .await?;
                    ctx.client
                        .wait_attr(&mp, "muted", 6, |v| *v == json!(true))
                        .await
                };
                let muted = match mute.await {
                    Ok(_) => "the receiver was muted".to_owned(),
                    Err(m) => format!("mute failed: {m}"),
                };
                return Err(Fail::new(format!(
                    "the volume after power on is not known after 2 tries: {e}; {muted}"
                )));
            }
        },
    };
    if v <= MAX_VOLUME {
        return Ok(String::new());
    }
    lower_volume(ctx, None).await.map_err(|e| {
        Fail::new(format!(
            "the volume after power on was {v} (limit {MAX_VOLUME}), not lowered: {e}"
        ))
    })?;
    Err(Fail::new(format!(
        "the volume after power on was {v} (limit {MAX_VOLUME}), it was lowered"
    )))
}

async fn ensure_on(ctx: &mut Ctx) -> Result<(), Fail> {
    let state = ctx.client.text(&ctx.mp(), "state");
    if !is_on(&state) {
        switch_on(ctx).await?;
    }
    Ok(())
}

/// Switch the receiver on. If it was on at the start, the volume is lowered to the start volume
/// with [`on_again`] (to [`MAX_VOLUME`] if the start volume is not known); else [`power_on`]
/// lowers it to [`MAX_VOLUME`].
async fn switch_on(ctx: &mut Ctx) -> Result<(), Fail> {
    match ctx.start.as_ref().filter(|_| ctx.was_on).map(|s| s.volume) {
        Some(volume) => on_again(ctx, volume).await,
        None => power_on(ctx).await.map(|_| ()),
    }
}

/// Select an option of a select entity and wait for the sensor of the same value.
async fn change_select(ctx: &mut Ctx, kind: &str, secs: u64) -> Result<(String, String), Fail> {
    let select = ctx.select(kind);
    let current = ctx.client.text(&select, "current_option");
    let options = strings(ctx.client.attr(&select, "options").unwrap_or(&Value::Null));
    let Some(other) = options
        .iter()
        .find(|o| **o != current && *o != "--")
        .cloned()
    else {
        return Err(Fail::skip(format!(
            "{kind} has no other option: {options:?}"
        )));
    };
    ctx.client
        .command_ok(
            "select",
            &select,
            "select_option",
            json!({ "option": other }),
        )
        .await?;
    ctx.client
        .wait_text(&select, "current_option", secs, &other)
        .await?;
    Ok((current, other))
}

async fn set_option(ctx: &mut Ctx, kind: &str, option: &str, secs: u64) -> Result<(), Fail> {
    let select = ctx.select(kind);
    ctx.client
        .command_ok(
            "select",
            &select,
            "select_option",
            json!({ "option": option }),
        )
        .await?;
    ctx.client
        .wait_text(&select, "current_option", secs, option)
        .await
}

/// Join the result of a check with the result of the step that sets the receiver or the driver
/// back. The error of the check comes first. A failed set back is always a failure, also after a
/// skip.
fn first_error<T>(body: Result<T, Fail>, undo: Result<(), Fail>) -> Result<T, Fail> {
    match (body, undo) {
        (body, Ok(())) => body,
        (Ok(_), Err(undo)) => Err(Fail::new(format!("not set back: {undo}"))),
        (Err(body), Err(undo)) => Err(Fail::new(format!("{body}; not set back: {undo}"))),
    }
}

/// The highest volume that a set back or an unmute may leave: not above the volume before the
/// change and not above [`MAX_VOLUME`].
fn unmute_limit(before: Option<f64>) -> f64 {
    before.map_or(MAX_VOLUME, |b| b.min(MAX_VOLUME))
}

/// The telnet command that sets the volume of the main zone to `target`, on the scale of the
/// Remote: `MV35` for 35, `MV355` for 35.5. `None` for a value that is not a whole or a half
/// number, or that is outside 0 to [`MAX_VOLUME`].
fn mv_command(target: f64) -> Option<String> {
    if !(0.0..=MAX_VOLUME).contains(&target) || (target * 2.0).fract() != 0.0 {
        return None;
    }
    let whole = target.trunc();
    Some(if target == whole {
        format!("MV{whole:02}")
    } else {
        format!("MV{whole:02}5")
    })
}

/// How a volume is sent.
#[derive(Debug, PartialEq)]
enum VolumePlan {
    /// A volume command of the media player.
    Volume(f64),
    /// A telnet command through the remote entity. If the driver refuses it, `fallback`, the next
    /// lower whole number, goes as a volume command of the media player.
    Raw { command: String, fallback: f64 },
}

/// How to send the volume `target`. A whole number goes as a volume command. A half number goes
/// as a telnet command. Another number goes as the next lower whole number. Fails for a volume
/// that is above [`MAX_VOLUME`], below 0 or not a number.
fn volume_plan(target: f64) -> Result<VolumePlan, Fail> {
    ensure!(
        (0.0..=MAX_VOLUME).contains(&target),
        "volume {target} is not in 0 to {MAX_VOLUME}, not sent"
    );
    if target.fract() == 0.0 {
        return Ok(VolumePlan::Volume(target));
    }
    Ok(match mv_command(target) {
        Some(command) => VolumePlan::Raw {
            command,
            fallback: target.floor(),
        },
        None => VolumePlan::Volume(target.floor()),
    })
}

/// Set the volume by the plan of [`volume_plan`]. Returns the volume that was sent.
async fn set_volume(ctx: &mut Ctx, target: f64) -> Result<f64, Fail> {
    let mp = ctx.mp();
    let sent = match volume_plan(target)? {
        VolumePlan::Volume(v) => v,
        VolumePlan::Raw { command, fallback } => {
            let remote = ctx.remote();
            let answer = ctx
                .client
                .command("remote", &remote, "send_cmd", json!({ "command": command }))
                .await;
            if answer.is_ok_and(|(response, _)| response["code"] == 200) {
                return Ok(target);
            }
            fallback
        }
    };
    ctx.client
        .command_ok("media_player", &mp, "volume", json!({ "volume": sent }))
        .await?;
    Ok(sent)
}

/// The volume to send when the receiver has `fresh` and the limit is `limit`: the limit if the
/// volume is above it, else nothing.
fn volume_to_send(fresh: f64, limit: f64) -> Option<f64> {
    (fresh > limit).then_some(limit)
}

/// The volume `v` is above `baseline`, the volume at the start, or above [`MAX_VOLUME`].
fn above_baseline(v: f64, baseline: Option<f64>) -> bool {
    v > baseline.map_or(MAX_VOLUME, |b| b.min(MAX_VOLUME))
}

/// Join two results, for example of set back steps. All errors are kept, the first one first.
fn join_errors(a: Result<(), Fail>, b: Result<(), Fail>) -> Result<(), Fail> {
    match (a, b) {
        (Ok(()), b) => b,
        (a, Ok(())) => a,
        (Err(a), Err(b)) => Err(Fail::new(format!("{a}; {b}"))),
    }
}

/// Lower the volume to [`unmute_limit`] if the receiver has a higher one. Only the volume of the
/// receiver (second connection) decides if a volume is sent: the driver can show an old, higher
/// volume, and the limit can be above the real volume. Without that connection nothing is sent,
/// and a driver volume above the limit is an error.
async fn lower_volume(ctx: &mut Ctx, before: Option<f64>) -> Result<(), Fail> {
    let mp = ctx.mp();
    let limit = unmute_limit(before);
    let Some(now) = receiver_volume(ctx).await? else {
        let shown = driver_volume(ctx)
            .await?
            .ok_or_else(|| Fail::new("no volume"))?;
        ensure!(
            shown <= limit,
            "volume {shown} above {limit}, not lowered: no second connection"
        );
        return Ok(());
    };
    let Some(limit) = volume_to_send(now, limit) else {
        return Ok(());
    };
    set_volume(ctx, limit).await?;
    ctx.client
        .wait_attr(&mp, "volume", 6, |v| v.as_f64().is_some_and(|x| x <= limit))
        .await
        .map_err(|e| Fail::new(format!("volume not lowered to {limit}: {e}")))?;
    let after = receiver_volume(ctx).await?.unwrap_or(limit);
    ensure!(after <= limit, "volume {after} after lowering to {limit}");
    Ok(())
}

/// Unmute, but first lower the volume with [`lower_volume`]. Unmutes only if the higher of the
/// receiver and the driver volume is at or below the limit.
async fn unmute_safely(ctx: &mut Ctx, before: Option<f64>) -> Result<(), Fail> {
    let mp = ctx.mp();
    let limit = unmute_limit(before);
    lower_volume(ctx, before)
        .await
        .map_err(|e| Fail::new(format!("{e}, not unmuted")))?;
    let now = highest_volume(ctx)
        .await
        .map_err(|e| Fail::new(format!("{e}, not unmuted")))?;
    ensure!(now <= limit, "volume {now} above {limit}, not unmuted");
    ctx.client
        .command_ok("media_player", &mp, "unmute", json!({}))
        .await?;
    ctx.client
        .wait_attr(&mp, "muted", 6, |v| *v == json!(false))
        .await?;
    Ok(())
}

/// Set the mute state to `muted`. Without `force`, nothing is sent if the driver shows that
/// state. An unmute goes through [`unmute_safely`].
async fn set_muted(
    ctx: &mut Ctx,
    muted: bool,
    before: Option<f64>,
    force: bool,
) -> Result<(), Fail> {
    let mp = ctx.mp();
    ctx.client.listen_ms(300).await?;
    if !force && ctx.client.attr(&mp, "muted") == Some(&json!(muted)) {
        return Ok(());
    }
    if !muted {
        return unmute_safely(ctx, before).await;
    }
    ctx.client
        .command_ok("media_player", &mp, "mute", json!({}))
        .await?;
    ctx.client
        .wait_attr(&mp, "muted", 6, |v| *v == json!(true))
        .await?;
    Ok(())
}

/// Set a select back to `old`. Without `force`, nothing is sent if the select shows `old`.
/// Does nothing without a real old option.
async fn select_back(
    ctx: &mut Ctx,
    kind: &str,
    old: &str,
    secs: u64,
    force: bool,
) -> Result<(), Fail> {
    if old.is_empty() || old == "--" {
        return Ok(());
    }
    ctx.client.listen_ms(300).await?;
    if !force && ctx.client.text(&ctx.select(kind), "current_option") == old {
        return Ok(());
    }
    set_option(ctx, kind, old, secs)
        .await
        .map_err(|e| Fail::new(format!("{kind} back to {old}: {e}")))
}

// ---------------------------------------------------------------- setup

pub async fn metadata(ctx: &mut Ctx) -> Check {
    let version = ctx
        .client
        .request_ok("get_driver_version", Value::Null)
        .await?;
    let metadata = ctx
        .client
        .request_ok("get_driver_metadata", Value::Null)
        .await?;
    ensure!(
        metadata["driver_id"] == "denonavr_rs",
        "driver_id is {}",
        metadata["driver_id"]
    );
    ensure!(
        metadata["setup_data_schema"].is_object(),
        "no setup_data_schema"
    );
    Ok(format!("driver {}", version["version"]["driver"]))
}

pub async fn setup_first(ctx: &mut Ctx) -> Check {
    let address = ctx.opt.address.clone();
    ctx.client
        .request_ok(
            "setup_driver",
            json!({ "reconfigure": false, "setup_data": {} }),
        )
        .await?;
    let page = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&page).contains(&"restore_from_backup".to_owned()),
        "first page: {page}"
    );
    ctx.client
        .answer(json!({ "debug_log": "true", "restore_from_backup": "false" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&page).contains(&"address".to_owned()),
        "discover page: {page}"
    );
    // An address with a space is refused and the page comes again.
    ctx.client
        .answer(json!({ "address": "not an address" }))
        .await?;
    let again = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&again).contains(&"address".to_owned()),
        "no discover page after a bad address: {again}"
    );
    ctx.client.answer(json!({ "address": address })).await?;
    let page = ctx.client.next_page(30).await?;
    ensure!(
        field_ids(&page).contains(&"choice".to_owned()),
        "device page: {page}"
    );
    let dirac = field_value(&page, "is_dirac_supported").unwrap_or_default();
    let choice = device_choice(&page, &address)
        .ok_or_else(|| Fail::new(format!("no device {address} on the device page: {page}")))?;
    ctx.client.answer(json!({ "choice": choice })).await?;
    ctx.client.setup_done(20).await?;
    Ok(format!("Dirac in the setup: {dirac}"))
}

/// The id of the receiver on the device page of the setup: the item with `address` as id, else the
/// only item. For a host name, the driver lists the receiver by its IP address.
fn device_choice(page: &Value, address: &str) -> Option<String> {
    let items = page["settings"]
        .as_array()?
        .iter()
        .find(|s| s["id"] == "choice")?["field"]["dropdown"]["items"]
        .as_array()?;
    let mut ids = items.iter().filter_map(|item| item["id"].as_str());
    if let Some(id) = ids.clone().find(|id| *id == address) {
        return Some(id.to_owned());
    }
    match (ids.next(), ids.next()) {
        (Some(id), None) => Some(id.to_owned()),
        _ => None,
    }
}

/// An entity id has only ASCII letters, digits and `-`, `_` and `.`, and is not empty.
fn valid_entity_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// The entity ids that are not valid. A missing id is shown as `?`.
fn invalid_ids(list: &[Value]) -> Vec<&str> {
    list.iter()
        .map(|e| e["entity_id"].as_str().unwrap_or("?"))
        .filter(|id| !valid_entity_id(id))
        .collect()
}

/// The ids of the entities without an English name.
fn unnamed(list: &[Value]) -> Vec<&str> {
    list.iter()
        .filter(|e| e["name"]["en"].as_str().is_none_or(str::is_empty))
        .map(|e| e["entity_id"].as_str().unwrap_or("?"))
        .collect()
}

pub async fn entities(ctx: &mut Ctx) -> Check {
    let list = entity_list(ctx).await?;
    let mp = list
        .iter()
        .filter_map(|e| e["entity_id"].as_str())
        .find(|id| id.starts_with("media_player."))
        .ok_or_else(|| Fail::new("no media player entity"))?;
    ctx.key = mp.trim_start_matches("media_player.").to_owned();
    read_tuner(ctx).await?;
    let count = |ty: &str| list.iter().filter(|e| e["entity_type"] == ty).count();
    let (selects, sensors) = (count("select"), count("sensor"));
    ensure!(count("remote") == 1, "remote entities: {}", count("remote"));
    let name_of = |id: String| {
        list.iter()
            .find(|e| e["entity_id"] == id.as_str())
            .map(|e| e["name"]["en"].as_str().unwrap_or_default().to_owned())
    };
    let volume_name =
        name_of(ctx.sensor("volume_db")).ok_or_else(|| Fail::new("no volume sensor"))?;
    ensure!(
        volume_name.ends_with("Volume dB"),
        "volume sensor name: {volume_name}"
    );
    ensure!(
        name_of(ctx.select("input_source")).is_some() && name_of(ctx.select("dimmer")).is_some(),
        "input source or dimmer select is missing"
    );
    ensure!(
        sensors > 7 && selects > 6,
        "{selects} selects, {sensors} sensors"
    );
    let bad = invalid_ids(&list);
    ensure!(bad.is_empty(), "invalid entity ids: {bad:?}");
    let unnamed = unnamed(&list);
    ensure!(
        unnamed.is_empty(),
        "entities without an English name: {unnamed:?}"
    );
    Ok(format!(
        "key {}, {selects} selects, {sensors} sensors",
        ctx.key
    ))
}

pub async fn states(ctx: &mut Ctx) -> Check {
    ctx.client.send_event("connect").await?;
    let count = subscribe_all(ctx).await?;
    let state = wait_available(ctx, 20).await?;
    ctx.was_on = is_on(&state);
    ctx.client.listen(2).await?;
    let mp = ctx.mp();
    // The driver values first: `restore_power` needs them if a later part of this step fails.
    ctx.start = Some(crate::Start {
        muted: ctx.client.attr(&mp, "muted").and_then(Value::as_bool),
        volume: current_volume(ctx),
    });
    read_tuner(ctx).await?;
    let address = ctx.opt.address.clone();
    let connect = async {
        if ctx.opt.no_oracle {
            Err(Fail::new("not wanted"))
        } else {
            Oracle::connect(&address).await
        }
    };
    let oracle = match connect.await {
        Ok(oracle) => {
            ctx.oracle = Some(oracle);
            // The second telnet connection must not push the driver out.
            ctx.client.listen(3).await?;
            // Read the start volume again, with the volume of the receiver.
            if ctx.was_on
                && let Ok(volume) = baseline_volume(ctx).await
                && let Some(start) = &mut ctx.start
            {
                start.volume = volume;
            }
            // A mute state that the receiver and the driver do not agree on is not known.
            let receiver_muted = ctx
                .oracle
                .as_ref()
                .and_then(|oracle| oracle.state().zone(Zone::Main).muted);
            if let Some(start) = &mut ctx.start
                && start.muted != receiver_muted
            {
                start.muted = None;
            }
            let after = ctx.client.text(&mp, "state");
            ensure!(
                after != "UNAVAILABLE",
                "the driver lost the receiver when a second telnet connection started"
            );
            "values are compared with a second connection".to_owned()
        }
        Err(e) => format!("NO second connection ({e}): values are not compared with the receiver"),
    };
    let unknown: Vec<&str> = [
        "tone_control",
        "dynamic_eq",
        "trigger_1",
        "bluetooth_transmitter",
    ]
    .into_iter()
    .filter(|k| ctx.client.text(&format!("switch.{k}.{}", ctx.key), "state") == "UNKNOWN")
    .collect();
    Ok(format!(
        "{count} entities, state {state}, volume {}, source {}, {oracle}, switches without a value: {unknown:?}",
        ctx.client.text(&mp, "volume"),
        ctx.client.text(&mp, "source")
    ))
}

pub async fn values(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let c = &ctx.client;
    let source = c.text(&mp, "source");
    ensure!(!source.is_empty(), "no source");
    ensure!(
        strings(c.attr(&mp, "source_list").unwrap_or(&Value::Null)).contains(&source),
        "source {source} is not in the source list"
    );
    ensure!(
        c.text(&ctx.sensor("input_source"), "value") == source,
        "input source sensor: {}",
        c.text(&ctx.sensor("input_source"), "value")
    );
    ensure!(
        c.text(&ctx.select("input_source"), "current_option") == source,
        "input source select: {}",
        c.text(&ctx.select("input_source"), "current_option")
    );
    let vol = current_volume(ctx).ok_or_else(|| Fail::new("no volume"))?;
    let sensor = ctx
        .client
        .attr(&ctx.sensor("volume_absolute"), "value")
        .and_then(Value::as_f64);
    ensure!(
        sensor == Some(vol),
        "absolute volume sensor {sensor:?}, player {vol}"
    );
    let muted = ctx.client.attr(&mp, "muted") == Some(&json!(true));
    let mute = ctx.client.text(&ctx.sensor("mute"), "value");
    ensure!(
        mute == if muted { "on" } else { "off" },
        "mute sensor {mute}, player muted {muted}"
    );
    verify_fresh(ctx).await?;
    let empty: Vec<String> = SENSORS
        .iter()
        .filter(|k| ctx.client.text(&ctx.sensor(k), "value") == "--")
        .map(|k| (*k).to_owned())
        .collect();
    Ok(format!(
        "volume {vol}, source {source}, sensors without a value: {empty:?}"
    ))
}

pub async fn remote_ui(ctx: &mut Ctx) -> Check {
    let list = entity_list(ctx).await?;
    let id = ctx.remote();
    let remote = list
        .iter()
        .find(|e| e["entity_id"] == id.as_str())
        .ok_or_else(|| Fail::new("no remote entity"))?;
    let options = &remote["options"];
    let simple = options["simple_commands"].as_array().map_or(0, Vec::len);
    let pages = options["user_interface"]["pages"]
        .as_array()
        .map_or(0, Vec::len);
    // A page is left out when the driver offers none of its buttons. A button of a command that is
    // not in the table of simple commands always stays.
    let config = read_config(ctx).await?;
    let device = config_device(ctx, &config)?;
    let slots = device["info"]["quick_select_count"].as_u64();
    let offered: BTreeSet<&str> = options["simple_commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let table: BTreeSet<&str> = COMMANDS
        .lines()
        .filter_map(|l| l.split('\t').next())
        .collect();
    let ui: Value =
        serde_json::from_str(UI_PAGES).map_err(|e| Fail::new(format!("remote_ui.json: {e}")))?;
    let brand = if device["is_denon"] == false {
        "marantz"
    } else {
        "denon"
    };
    let expected = ui[brand]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|page| {
            page["items"].as_array().into_iter().flatten().any(|item| {
                item["command"]["cmd_id"]
                    .as_str()
                    .is_none_or(|c| !table.contains(c) || offered.contains(c))
            })
        })
        .count();
    let quick = options["user_interface"]["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["items"].as_array().into_iter().flatten())
        .filter_map(|i| i["command"]["cmd_id"].as_str())
        .filter(|c| c.starts_with("QUICK_SELECT_") || c.starts_with("SMART_SELECT_"))
        .count();
    ensure!(simple > 300, "{simple} simple commands");
    ensure!(pages == expected, "{pages} UI pages, expected {expected}");
    // The page count follows the offered commands. These rules are checked on their own.
    let has = |prefix: &str| offered.iter().any(|c| c.starts_with(prefix));
    ensure!(
        has_tuner(device) || !has("TUNER_"),
        "tuner commands without a tuner"
    );
    ensure!(
        device["is_dirac_supported"] == true || !has("DIRAC_"),
        "Dirac commands without Dirac"
    );
    let picture = ctx.oracle.as_ref().map(|oracle| {
        let features = oracle.avr().info().features;
        features.is_none_or(|f| f.has(denon_marantz::DeviceFeature::PictureMode))
    });
    ensure!(
        picture != Some(false) || !has("PICTURE_MODE_"),
        "picture mode commands, but the receiver does not list picture mode"
    );
    ensure!(
        slots.is_none_or(|n| usize::try_from(n).is_ok_and(|n| n.min(6) == quick)),
        "{quick} quick select buttons, the receiver has {slots:?}"
    );
    ensure!(options["button_mapping"].is_array(), "no button mapping");
    let features = strings(&remote["features"]);
    ensure!(
        features.contains(&"send_cmd".to_owned()),
        "features: {features:?}"
    );
    Ok(format!(
        "{simple} simple commands, {pages} pages, {quick} quick select buttons"
    ))
}

// ---------------------------------------------------------------- wake and sleep

/// Part of a driver log line for a full connect.
const CONNECTED_IN: &str = "connected in";

pub async fn standby(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    ctx.client.send_event("enter_standby").await?;
    let asleep = async {
        ctx.client.listen(3).await?;
        let state = ctx.client.text(&mp, "state");
        ensure!(state != "UNAVAILABLE", "state in standby: {state}");
        Ok(())
    }
    .await;
    if let Err(e) = asleep {
        return first_error(Err(e), wake_again(ctx).await);
    }
    let mark = ctx.driver.log_size().await;
    ctx.client.send_event("exit_standby").await?;
    let start = Instant::now();
    while !ctx.driver.log_from(mark).await.contains(CONNECTED_IN) {
        ensure!(
            start.elapsed() < Duration::from_secs(8),
            "no new connection after exit_standby"
        );
        ctx.client.listen(1).await?;
    }
    let took = start.elapsed().as_millis();
    ctx.client.refresh_states().await?;
    let state = wait_available(ctx, 5).await?;
    ctx.client.listen(3).await?;
    verify_fresh(ctx).await?;
    Ok(format!(
        "connected {took} ms after exit_standby, state {state}"
    ))
}

pub async fn remote_wake(ctx: &mut Ctx) -> Check {
    let start = Instant::now();
    ctx.reconnect().await?;
    ctx.client.refresh_states().await?;
    let first = ctx.client.text(&ctx.mp(), "state");
    let took = start.elapsed().as_millis();
    ensure!(
        first != "UNAVAILABLE",
        "first state after a new connect: {first}"
    );
    subscribe_all(ctx).await?;
    Ok(format!("state {first} after {took} ms"))
}

/// The waits in ms in [`quick_wake`]: from a wake-up to the next standby, and from that standby
/// to the next wake-up. A connect takes about 30 to 60 ms. A short first wait sends the standby
/// while the connect still runs. A long first wait with a short second wait sends the wake-up
/// while the old link still closes.
const QUICK_GAPS_MS: [(u64, u64); 6] =
    [(0, 0), (20, 20), (100, 0), (100, 5), (100, 20), (400, 400)];

/// The status names of the driver.
const STATUSES: [&str; 4] = ["Disconnected", "Connecting", "Connected", "Error"];

/// The new status of a status line of the driver log, for example `Connected`.
fn new_status(line: &str) -> Option<&str> {
    if !line.contains("] status ") {
        return None;
    }
    let (_, status) = line.rsplit_once(" -> ")?;
    let status = status.trim();
    STATUSES.contains(&status).then_some(status)
}

/// The status of the last status line of the driver log.
fn last_status(log: &str) -> Option<&str> {
    log.lines().rev().find_map(new_status)
}

/// The number of status lines of the driver log that change to `status`.
fn status_count(log: &str, status: &str) -> usize {
    log.lines()
        .filter(|l| new_status(l) == Some(status))
        .count()
}

/// Parts of driver log lines for a failed connect of a receiver.
const FAILED_LINES: [&str; 2] = ["] connect failed: ", "] not found on the network"];

/// Part of a driver log line for a connect that a disconnect stopped.
const CANCEL_LINE: &str = "] connect canceled";

/// The number of failed receiver connects in the driver log.
fn failed_connects(log: &str) -> usize {
    count_lines(log, &FAILED_LINES)
}

/// The number of canceled receiver connects in the driver log.
fn canceled_connects(log: &str) -> usize {
    log.matches(CANCEL_LINE).count()
}

/// The time of the last full connect in the driver log, in ms.
fn connect_ms(log: &str) -> Option<u64> {
    let (_, rest) = log.rsplit_once("] connected in ")?;
    rest.split_once(" ms")?.0.parse().ok()
}

/// Driver log line for a telnet link that did not open at the connect. The driver tries again.
const TELNET_RETRY: &str = "] telnet not open, trying again";

/// Checks the driver log after the last wake-up of [`quick_wake`]: no failed connect since the
/// step started, then exactly one connect and no drop. The telnet link must not be lost and must be
/// open again after a telnet retry. With `telnet`, it must open. Returns the number of telnet
/// retry lines after the last wake-up. The driver logs at most one for each connect.
fn after_wake(tail: &str, failed: usize, telnet: bool) -> Result<usize, String> {
    if failed > 0 {
        return Err(format!("connects failed: {failed}"));
    }
    let connects = status_count(tail, "Connected");
    let drops = status_count(tail, "Disconnected") + status_count(tail, "Error");
    if connects != 1 || drops != 0 {
        return Err(format!(
            "after the last exit_standby: {connects} connects, {drops} drops"
        ));
    }
    if LOSS_LINES.iter().any(|l| tail.contains(&format!("] {l}"))) {
        return Err("telnet link lost after the last exit_standby".to_owned());
    }
    if let Some(at) = tail.rfind(TELNET_RETRY)
        && !tail[at..].contains("] telnet connected")
    {
        return Err("telnet not open after the last exit_standby".to_owned());
    }
    if telnet && !tail.contains("] telnet connected") {
        return Err("telnet not connected after the last exit_standby".to_owned());
    }
    Ok(tail.matches(TELNET_RETRY).count())
}

/// Wait up to `limit` until the last status line of the driver log after byte `from` shows
/// `status`. Status lines before `from` do not count. Returns the byte offset after the whole
/// lines in which it was seen.
async fn wait_status(ctx: &mut Ctx, from: u64, status: &str, limit: Duration) -> Result<u64, Fail> {
    debug_assert!(STATUSES.contains(&status), "{status}");
    let start = Instant::now();
    loop {
        let (log, end) = ctx.driver.log_lines(from).await;
        let last = last_status(&log);
        if last == Some(status) {
            return Ok(end);
        }
        ensure!(
            start.elapsed() < limit,
            "status {}, not {status}, after {} s",
            last.unwrap_or("none"),
            limit.as_secs()
        );
        ctx.client.listen_ms(200).await?;
    }
}

/// Wake the Remote up after a failed step, else the next checks run in standby. Waits until the
/// driver is connected after the wake-up, see [`woke`]. When the driver looks connected 1 s after
/// the wake-up, it checks again 2 s later. When it is not connected at either check, it waits up
/// to 7 s for `Connected`.
async fn wake_again(ctx: &mut Ctx) -> Result<(), Fail> {
    let (before, from) = ctx
        .driver
        .find_last(|log| last_status(log).map(str::to_owned))
        .await;
    ctx.client.send_event("exit_standby").await?;
    ctx.client.listen_ms(1000).await?;
    let (tail, _) = ctx.driver.log_lines(from).await;
    if woke(before.as_deref(), last_status(&tail)) {
        ctx.client.listen_ms(2000).await?;
        let (tail, _) = ctx.driver.log_lines(from).await;
        if woke(before.as_deref(), last_status(&tail)) {
            return Ok(());
        }
    }
    wait_status(ctx, from, "Connected", Duration::from_secs(7))
        .await
        .map(drop)
}

/// Whether the driver is connected after a wake-up: the last status line after it is
/// `Connected`, or there is none and the last one before it is `Connected`.
fn woke(before: Option<&str>, after: Option<&str>) -> bool {
    after.or(before) == Some("Connected")
}

/// The standby and wake-up events of [`quick_wake`], up to the last standby. `from` is the byte
/// offset in the driver log where the step starts. Returns the byte offset of the driver log after
/// the lines in which the driver is disconnected.
async fn standby_burst(ctx: &mut Ctx, from: u64) -> Result<u64, Fail> {
    ctx.client.send_event("enter_standby").await?;
    wait_status(ctx, from, "Disconnected", Duration::from_secs(5)).await?;
    for (awake, asleep) in QUICK_GAPS_MS {
        ctx.client.send_event("exit_standby").await?;
        ctx.client.listen_ms(awake).await?;
        ctx.client.send_event("enter_standby").await?;
        ctx.client.listen_ms(asleep).await?;
    }
    wait_status(ctx, from, "Disconnected", Duration::from_secs(3)).await
}

/// The Remote wakes up and goes to standby again with short gaps. With the short gaps, a standby
/// can come while a connect still runs, and a wake-up while the old link still closes. After the
/// last standby the driver must disconnect from the receiver. No connect may fail during the
/// step. After the last wake-up the driver must connect once, stay connected and show the values
/// of the receiver. If the step fails before the last wake-up, the Remote wakes up again. The
/// result shows the connect time of the driver, how many connects were canceled in the step and
/// how many telnet retry lines came after the last wake-up.
pub async fn quick_wake(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    // The end of the whole lines, so the step text does not start inside a line.
    let start = ctx.driver.line_end().await;
    ensure!(start > 0, "the driver log has no whole line");
    let config = read_config(ctx).await?;
    let telnet = config_device(ctx, &config)?["use_telnet"] != false;
    let from = match standby_burst(ctx, start).await {
        Ok(from) => from,
        Err(e) => return first_error(Err(e), wake_again(ctx).await),
    };
    ctx.client.send_event("exit_standby").await?;
    wait_status(ctx, from, "Connected", Duration::from_secs(8)).await?;
    ctx.client.listen(2).await?;
    let step = ctx.driver.log_from(start).await;
    let tail = ctx.driver.log_from(from).await;
    let failed = failed_connects(&step);
    let retries = after_wake(&tail, failed, telnet).map_err(Fail::new)?;
    let ms = connect_ms(&tail).map_or_else(|| "?".to_owned(), |ms| ms.to_string());
    let canceled = canceled_connects(&step);
    ctx.client.refresh_states().await?;
    let state = wait_available(ctx, 5).await?;
    verify_fresh(ctx).await?;
    Ok(format!(
        "driver connect {ms} ms, state {state}, {canceled} canceled connects, \
         {retries} telnet retry lines after the last exit_standby"
    ))
}

/// The longest wait for the driver to connect again after the network restart of the receiver.
const RESTART_LIMIT: Duration = Duration::from_secs(180);

/// Parts of driver log lines for a lost telnet link: from the driver and from the library.
const LOSS_LINES: [&str; 2] = ["telnet disconnected", "telnet connection lost"];

/// Parts of driver log lines for a connect: full connects of the driver, and telnet connects of
/// the driver and the library.
const CONNECT_LINES: [&str; 2] = ["] connected in", "telnet connected"];

fn count_lines(log: &str, parts: &[&str]) -> usize {
    parts.iter().map(|p| log.matches(p).count()).sum()
}

/// Where the last loss line of `log` starts, if any.
fn last_loss(log: &str) -> Option<usize> {
    LOSS_LINES.iter().filter_map(|p| log.rfind(p)).max()
}

/// A connect line comes after byte `from` of `log`.
fn connected_after(log: &str, from: usize) -> bool {
    log.get(from..)
        .is_some_and(|rest| CONNECT_LINES.iter().any(|p| rest.contains(p)))
}

/// Restart the network of the receiver with the `NETWORK_RESTART` command, like a receiver that
/// restarts. The driver must connect again and show the values of the receiver. The second
/// connection is opened again. Only with `--only receiver_restart`.
pub async fn receiver_restart(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    // Start at a line end, so the first line read is whole.
    let mark = ctx.driver.line_end().await;
    let remote = ctx.remote();
    ctx.client
        .command_ok(
            "remote",
            &remote,
            "send_cmd",
            json!({ "command": "NETWORK_RESTART" }),
        )
        .await?;
    let start = Instant::now();
    let mp = ctx.mp();
    // Only a connect line after the last loss line counts: the link must go down first.
    let mut from: Option<usize> = None;
    let mut lost = 0;
    loop {
        let (log, _) = ctx.driver.log_lines(mark).await;
        if let Some(at) = last_loss(&log) {
            from = Some(at);
        } else if from.is_none() && ctx.client.text(&mp, "state") == "UNAVAILABLE" {
            from = Some(log.len());
        }
        if from.is_some() && lost == 0 {
            lost = start.elapsed().as_secs().max(1);
        }
        if from.is_some_and(|at| connected_after(&log, at)) {
            break;
        }
        ensure!(
            start.elapsed() < RESTART_LIMIT,
            "{} {} s after the network restart",
            if from.is_none() {
                "the connection was not lost".to_owned()
            } else {
                format!("lost after {lost} s, no new connection")
            },
            RESTART_LIMIT.as_secs()
        );
        ctx.client.listen(1).await?;
    }
    let took = start.elapsed().as_secs();
    // The new second connection replaces the old one only when it works: the old one connects
    // again by itself, and restore_power needs one to lower the volume.
    if ctx.oracle.is_some() {
        let address = ctx.opt.address.clone();
        let mut tries = 0;
        let new = loop {
            tries += 1;
            match Oracle::connect(&address).await {
                Ok(oracle) => break oracle,
                Err(e) if tries >= 10 => {
                    return Err(Fail::new(format!(
                        "the driver connected again after {took} s, a new second connection failed: {e}. The old one is kept"
                    )));
                }
                Err(_) => sleep(Duration::from_secs(3)).await,
            }
        };
        if let Some(old) = ctx.oracle.replace(new) {
            old.close().await;
        }
    }
    wait_available(ctx, 20).await?;
    ctx.client.listen(3).await?;
    verify_fresh(ctx).await?;
    Ok(format!(
        "lost after {lost} s, connected again {took} s after the network restart"
    ))
}

// ---------------------------------------------------------------- control

pub async fn selects(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    // The options before the change, for the set back.
    let mut changed: Vec<(&str, String)> = Vec::new();
    let result = async {
        let mut note = Vec::new();
        for kind in ["dimmer", "eco_mode"] {
            changed.push((kind, ctx.client.text(&ctx.select(kind), "current_option")));
            let start = Instant::now();
            let (old, new) = change_select(ctx, kind, 6).await?;
            let sensor = ctx.sensor(kind);
            ctx.client.wait_text(&sensor, "value", 6, &new).await?;
            note.push(format!(
                "{kind} {old} -> {new} in {} ms",
                start.elapsed().as_millis()
            ));
            set_option(ctx, kind, &old, 6).await?;
            verify_all(ctx, 6).await?;
        }
        Ok(note.join(", "))
    }
    .await;
    let mut undo = Ok(());
    for (kind, old) in &changed {
        let back = select_back(ctx, kind, old, 6, result.is_err()).await;
        undo = undo.and(back);
    }
    first_error(result, undo)
}

pub async fn remote_commands(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let remote = ctx.remote();
    let dimmer = ctx.select("dimmer");
    let sensor = ctx.sensor("dimmer");
    let old = ctx.client.text(&dimmer, "current_option");
    let result = async {
        ctx.client
            .command_ok(
                "remote",
                &remote,
                "send_cmd",
                json!({ "command": "DIMMER_TOGGLE" }),
            )
            .await?;
        ctx.client
            .wait_attr(&sensor, "value", 6, |v| {
                v.as_str().is_some_and(|s| s != old)
            })
            .await?;
        set_option(ctx, "dimmer", &old, 6).await?;
        // A sequence of two commands, repeated.
        ctx.client
            .command_ok(
                "remote",
                &remote,
                "send_cmd_sequence",
                json!({ "sequence": ["DIMMER_TOGGLE", "DIMMER_TOGGLE"], "delay": 300 }),
            )
            .await?;
        ctx.client.listen(2).await?;
        set_option(ctx, "dimmer", &old, 6).await?;
        // The remote entity refuses an empty command.
        let (response, _) = ctx
            .client
            .command("remote", &remote, "send_cmd", json!({ "command": "" }))
            .await?;
        ensure!(
            response["code"] == 400,
            "empty command: code {}",
            response["code"]
        );
        verify_all(ctx, 6).await?;
        Ok(String::new())
    }
    .await;
    let undo = select_back(ctx, "dimmer", &old, 6, result.is_err()).await;
    first_error(result, undo)
}

/// Mute and unmute. Runs only if the receiver is not muted and its volume is at or below the
/// limit: the unmute at the end must not make it loud.
pub async fn mute(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    ctx.client.listen_ms(300).await?;
    match ctx.client.attr(&mp, "muted").and_then(Value::as_bool) {
        None => return Err(Fail::skip("the mute state is not known")),
        Some(true) => return Err(Fail::skip("the receiver is muted at the start")),
        Some(false) => {}
    }
    // One read of each value: the limit and the check use the same pair.
    let receiver = receiver_volume(ctx).await?;
    let driver = driver_volume(ctx).await?;
    if receiver.is_some() && receiver != driver {
        return Err(Fail::skip(format!(
            "the driver shows the volume {driver:?}, the receiver {receiver:?}: the driver value is old"
        )));
    }
    let before = lowest(receiver, driver);
    let high = highest(receiver, driver).ok_or_else(|| Fail::new("no volume"))?;
    if high > MAX_VOLUME {
        return Err(Fail::skip(format!(
            "the volume {high} is above the limit {MAX_VOLUME}"
        )));
    }
    let result = async {
        for target in [true, false] {
            if !target {
                let now = highest_volume(ctx).await?;
                let limit = unmute_limit(before);
                ensure!(
                    now <= limit,
                    "volume {now} above {limit} while muted, not unmuted by the check"
                );
            }
            ctx.client
                .command_ok("media_player", &mp, "mute_toggle", json!({}))
                .await?;
            ctx.client
                .wait_attr(&mp, "muted", 6, |v| *v == json!(target))
                .await?;
            let sensor = ctx.sensor("mute");
            let expected = if target { "on" } else { "off" };
            ctx.client.wait_text(&sensor, "value", 6, expected).await?;
            verify_all(ctx, 6).await?;
        }
        Ok(String::new())
    }
    .await;
    let undo = set_muted(ctx, false, before, result.is_err()).await;
    first_error(result, undo)
}

pub async fn source(ctx: &mut Ctx) -> Check {
    if ctx.opt.avoid_sources.is_empty() {
        return Err(Fail::skip("no --avoid-sources: no source is selected"));
    }
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let old = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    let sources = read_sources(ctx).await.map_err(Fail::skip)?;
    let new = other_source(&source_rule(ctx, &sources), &old, &list).map_err(Fail::skip)?;
    if let Some(why) = too_loud(ctx).await? {
        return Err(Fail::skip(format!("{why}: no source is selected")));
    }
    let result = async {
        let start = Instant::now();
        ctx.client
            .command_ok(
                "media_player",
                &mp,
                "select_source",
                json!({ "source": new }),
            )
            .await?;
        ctx.client
            .wait_text(&mp, "source", 8, &new)
            .await
            .map_err(|e| Fail::new(format!("{e}. Wanted: {new}, source list: {list:?}")))?;
        let took = start.elapsed().as_millis();
        ctx.client
            .wait_text(&ctx.select("input_source"), "current_option", 4, &new)
            .await?;
        let title = match source_title(&sources, &new) {
            Some(title) => {
                ctx.client
                    .wait_text(&mp, "media_title", 4, title)
                    .await
                    .map_err(|e| Fail::new(format!("title of {new}: {e}")))?;
                "the title is the source name"
            }
            None => "the title is not checked: a playing source, or the name fits no single source",
        };
        Ok((took, title))
    }
    .await;
    let back = restore_source(ctx, &old, 4).await;
    let tries = back.as_ref().copied().unwrap_or_default();
    let (took, title) = first_error(result, back.map(|_| ()))?;
    verify_all(ctx, 6).await?;
    let odd = if list.iter().any(|s| s == "Source") {
        ", the list has a `Source` entry"
    } else {
        ""
    };
    Ok(format!(
        "{old} -> {new} in {took} ms ({title}), back in {tries} tries{odd}"
    ))
}

/// The media title that the driver shows for the source with the name `label` of the driver list:
/// the name, for a source that does not report what plays. The rule of the driver checks the telnet
/// code. `None` for a source that reports what plays, or for a name that fits no source or more
/// than one.
fn source_title<'a>(sources: &[SourceInfo], label: &'a str) -> Option<&'a str> {
    let mut found = sources.iter().filter(|s| label_fits(s, label));
    match (found.next(), found.next()) {
        (Some(info), None) if !is_playing_source(&info.code) => Some(label),
        _ => None,
    }
}

/// A source of the driver list `list` to select for a check, or why there is none. The start
/// source `old` must be one that a check may select, because the set back selects it again. The
/// receiver lists a `Source` entry that cannot be selected: it is not used.
fn other_source(rule: &SourceRule<'_>, old: &str, list: &[String]) -> Result<String, String> {
    if rule.opt.avoid_sources.is_empty() {
        return Err("no --avoid-sources: no source is selected".to_owned());
    }
    start_source_ok(rule, old, list)?;
    list.iter()
        .find(|s| **s != old && *s != "Source" && rule.label_ok(s))
        .cloned()
        .ok_or_else(|| "only one source".to_owned())
}

/// The sources of the receiver from the second connection, with the default and automatic names
/// of the web interface. Error: why no source may be selected.
async fn read_sources(ctx: &Ctx) -> Result<Vec<SourceInfo>, String> {
    let oracle = source_oracle(ctx)?;
    oracle
        .read_source_names()
        .await
        .map_err(|e| format!("{e}: no source is selected"))?;
    let state = oracle.state();
    levels_ok(&state.device().source_levels).map_err(|e| format!("{e}: no source is selected"))?;
    Ok(state.device().sources.as_slice().to_vec())
}

/// The second connection that [`read_sources`] reads. Error: why no source may be selected, without
/// `--avoid-sources` or without that connection.
fn source_oracle(ctx: &Ctx) -> Result<&Oracle, String> {
    if ctx.opt.avoid_sources.is_empty() {
        return Err("no --avoid-sources: no source is selected".to_owned());
    }
    ctx.oracle
        .as_ref()
        .ok_or_else(|| "no second connection: no source is selected".to_owned())
}

/// Sources may be selected only while the receiver adds no level to the analog and the digital
/// inputs: another source can then not be louder at the same volume. Error if a level is above
/// 0 dB or not known.
fn levels_ok(levels: &SourceLevels) -> Result<(), String> {
    match (levels.analog_db, levels.digital_db) {
        (Some(analog), Some(digital)) if analog <= 0 && digital <= 0 => Ok(()),
        (Some(analog), Some(digital)) => Err(format!(
            "the input levels are above 0 dB (analog {analog} dB, digital {digital} dB)"
        )),
        _ => Err("the input levels are not known".to_owned()),
    }
}

/// Why the saved source `code` of an earlier run may not be set back, or `None` if it may: it and
/// the current source must both pass the source rule of this run. The receiver may have a tuner:
/// the setup has not run yet.
pub async fn source_set_back(oracle: &Oracle, opt: &crate::Options, code: &str) -> Option<String> {
    if opt.avoid_sources.is_empty() {
        return Some("no --avoid-sources".to_owned());
    }
    if opt.no_oracle {
        return Some("--no-oracle".to_owned());
    }
    if let Err(e) = oracle.read_source_names().await {
        return Some(e.msg);
    }
    let state = oracle.state();
    if let Err(e) = levels_ok(&state.device().source_levels) {
        return Some(e);
    }
    let rule = SourceRule {
        opt,
        sources: state.device().sources.as_slice(),
        tuner: true,
    };
    let current = state.zone(Zone::Main).source.clone();
    if !rule.code_ok(code) {
        Some(format!("the saved source {code} may not be selected"))
    } else if !current.as_deref().is_some_and(|c| rule.code_ok(c)) {
        Some(format!(
            "the source {} may not be changed",
            current.as_deref().unwrap_or("(unknown)")
        ))
    } else {
        None
    }
}

/// The source rule of the run for these sources.
fn source_rule<'a>(ctx: &'a Ctx, sources: &'a [SourceInfo]) -> SourceRule<'a> {
    SourceRule {
        opt: &ctx.opt,
        sources,
        tuner: ctx.tuner,
    }
}

/// Which sources the checks may select.
struct SourceRule<'a> {
    opt: &'a crate::Options,
    /// The sources of the receiver, from [`read_sources`].
    sources: &'a [SourceInfo],
    /// The receiver has a tuner.
    tuner: bool,
}

impl SourceRule<'_> {
    fn info(&self, code: &str) -> Option<&SourceInfo> {
        self.sources.iter().find(|s| s.code == code)
    }

    /// The source with this telnet code may be selected.
    fn code_ok(&self, code: &str) -> bool {
        self.info(code)
            .is_some_and(|info| may_send_source(self.opt, info, self.tuner))
    }

    /// The source with this name in the driver list may be selected. The driver has only the
    /// telnet names, so it shows the user name, else the code, and adds the code in parentheses
    /// when two sources have the same name. The default name also counts here: it can only make a
    /// name fit more than one source. A name that fits more than one source is not selected.
    fn label_ok(&self, label: &str) -> bool {
        let mut found = self.sources.iter().filter(|s| label_fits(s, label));
        match (found.next(), found.next()) {
            (Some(info), None) => may_send_source(self.opt, info, self.tuner),
            _ => false,
        }
    }
}

/// The driver can show the source with this name.
fn label_fits(info: &SourceInfo, label: &str) -> bool {
    let names = [
        info.name.as_deref(),
        info.default_name.as_deref(),
        Some(info.code.as_str()),
    ];
    let with_code = label
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" ("))
        .filter(|(_, code)| *code == info.code)
        .map(|(name, _)| name);
    names
        .into_iter()
        .flatten()
        .any(|name| name == label || with_code == Some(name))
}

/// Select the old source again. It is always sent: after an error the driver can show a source
/// that is old. The receiver can ignore a source command that comes soon after another one, so
/// the command is sent again after `secs` seconds.
async fn restore_source(ctx: &mut Ctx, old: &str, secs: u64) -> Result<u32, Fail> {
    // A source command switches the receiver on: switch it on with the volume check first.
    // Without the second connection, `power_on` skips, so no source is sent.
    ctx.client.listen_ms(300).await?;
    if receiver_off(ctx) || !is_on(&ctx.client.text(&ctx.mp(), "state")) {
        switch_on(ctx).await?;
    }
    let mp = ctx.mp();
    for tries in 1..=3 {
        ctx.client
            .command_ok(
                "media_player",
                &mp,
                "select_source",
                json!({ "source": old }),
            )
            .await?;
        if ctx.client.wait_text(&mp, "source", secs, old).await.is_ok() {
            return Ok(tries);
        }
    }
    Err(Fail::new(format!(
        "the source did not go back to {old} in 3 tries"
    )))
}

/// Why the start source `old` of the driver list `list` cannot be set back, or `Ok` if it can.
fn start_source_ok(rule: &SourceRule<'_>, old: &str, list: &[String]) -> Result<(), String> {
    if old.is_empty() || old == "Source" || !list.iter().any(|s| s == old) {
        return Err(format!(
            "the source {old:?} is not in the list, it could not be set back"
        ));
    }
    if !rule.label_ok(old) {
        return Err(format!(
            "the source {old} may not be selected, the set back would select it"
        ));
    }
    Ok(())
}

/// The HTTP poll time of the driver (`HTTP_POLL_INTERVAL` in `src/device/actor.rs`). In HTTP mode
/// the driver shows a change of the receiver only after its next poll.
const HTTP_POLL_SECS: u64 = 10;
/// Time for the driver to show a change after its signal check, after its poll or after a report
/// of the receiver.
const DRIVER_SLACK_SECS: u64 = 5;
/// Seconds that the driver may take to show a source that the owner selected.
const OWNER_BACK_SECS: u64 = HTTP_POLL_SECS + DRIVER_SLACK_SECS;

/// The result of a set back of the source.
#[derive(Debug, PartialEq)]
struct SetBack {
    /// Text for the note. Empty if there is nothing to say.
    note: String,
    /// Not set back: the owner left the source as it is. The check skips if nothing failed.
    skipped: bool,
}

impl SetBack {
    const fn note(note: String) -> Self {
        Self {
            note,
            skipped: false,
        }
    }
}

/// Add the set back `back` of [`source_back`] to the result of the check, as in
/// [`add_back_note`]. A skipped set back makes a passed check a skip.
fn add_set_back(result: Check, back: Result<SetBack, Fail>) -> Check {
    let back = match back {
        Ok(back) => back,
        Err(e) => return add_back_note(result, Err::<&str, _>(e)),
    };
    let result = if back.note.is_empty() {
        result
    } else {
        add_back_note(result, Ok(&back.note))
    };
    match result {
        Ok(note) if back.skipped => Err(Fail::skip(note)),
        result => result,
    }
}

/// The source of the main zone from the second connection: the telnet code and, if the receiver
/// lists it, the info of that source. The second connection has a change at once; in HTTP mode
/// the driver shows it only after its next poll. `None` without that connection or if the source
/// is not known.
fn receiver_source(ctx: &Ctx) -> Option<(String, Option<SourceInfo>)> {
    let state = ctx.oracle.as_ref()?.state();
    let code = state.zone(Zone::Main).source.clone()?;
    let info = state.device().sources.get(&code).cloned();
    Some((code, info))
}

/// The current source for a note: the name or the code from the second connection, else the
/// source that the driver shows.
fn source_text(ctx: &Ctx) -> String {
    match receiver_source(ctx) {
        Some((code, info)) => info.and_then(|i| i.name).unwrap_or(code),
        None => ctx.client.text(&ctx.mp(), "source"),
    }
}

/// Why the set back may not select a source at the volume `volume`, or `Ok` if it may: the volume
/// must be known and not above `start`, the volume at the start of the check, or [`MAX_VOLUME`].
fn back_volume_ok(volume: Option<f64>, start: f64) -> Result<(), String> {
    match volume {
        Some(v) if v <= MAX_VOLUME && v <= start => Ok(()),
        Some(v) if v <= MAX_VOLUME => {
            Err(format!("the volume {v} is above the start volume {start}"))
        }
        Some(v) => Err(format!("the volume {v} is above the limit {MAX_VOLUME}")),
        None => Err("the volume is not known".to_owned()),
    }
}

/// What [`source_back`] does.
#[derive(Debug, PartialEq)]
enum Back {
    /// The receiver has the start source: nothing is sent.
    Same,
    /// The check selects the start source.
    Select,
    /// The owner is asked, for this reason.
    Owner(String),
}

/// What [`source_back`] does with the start source `old` of the driver list `list`. The current
/// source comes from [`receiver_source`], else from the driver. To select `old`, the higher of the
/// receiver and the driver volume must pass [`back_volume_ok`] with `start_volume`, `old` must pass
/// [`start_source_ok`], and the current source must pass [`now_ok`].
async fn back_rule(
    ctx: &mut Ctx,
    old: &str,
    list: &[String],
    start_volume: f64,
) -> Result<Back, Fail> {
    ctx.client.listen_ms(300).await?;
    let receiver = receiver_source(ctx);
    let label = ctx.client.text(&ctx.mp(), "source");
    let now = receiver
        .as_ref()
        .map(|(code, info)| (code.as_str(), info.as_ref()));
    if same_source(now, &label, old) {
        return Ok(Back::Same);
    }
    // A failed read is an error: `source_back` asks the owner and fails.
    let volume = receiver_volume(ctx).await?;
    let driver = driver_volume(ctx).await?;
    if let Err(why) = back_volume_ok(highest(volume, driver), start_volume) {
        return Ok(Back::Owner(why));
    }
    let sources = match read_sources(ctx).await {
        Ok(sources) => sources,
        Err(why) => return Ok(Back::Owner(why)),
    };
    let rule = source_rule(ctx, &sources);
    let code = receiver.as_ref().map(|(code, _)| code.as_str());
    Ok(
        match start_source_ok(&rule, old, list).and_then(|()| now_ok(&rule, code, &label)) {
            Ok(()) => Back::Select,
            Err(why) => Back::Owner(why),
        },
    )
}

/// The receiver has the start source `old`, a name from the driver list. `receiver` is the code and
/// the info from [`receiver_source`], `label` the source that the driver shows; `label` counts
/// only without `receiver`. The receiver source has the name that the driver shows: the user name,
/// else the code, with the code in parentheses when two sources have the same name. The default
/// name does not count: the driver does not show it. If the receiver does not list the source,
/// the display name as in `oracle::expected`. False if `old` is empty.
fn same_source(receiver: Option<(&str, Option<&SourceInfo>)>, label: &str, old: &str) -> bool {
    if old.is_empty() {
        return false;
    }
    match receiver {
        Some((code, Some(info))) => {
            let name = info.name.as_deref().unwrap_or(code);
            old == name
                || old
                    .strip_suffix(')')
                    .and_then(|rest| rest.rsplit_once(" ("))
                    == Some((name, code))
        }
        Some((code, None)) => telnet_to_display_source(code) == old,
        None => label == old,
    }
}

/// Why the current source may not be changed by the rule `rule`, or `Ok` if it may. `code` is its
/// telnet code from the second connection, `label` the name that the driver shows. The code
/// decides if it is known: the driver can show an old source.
fn now_ok(rule: &SourceRule<'_>, code: Option<&str>, label: &str) -> Result<(), String> {
    let ok = match code {
        Some(code) => rule.code_ok(code),
        None => rule.label_ok(label),
    };
    if ok {
        return Ok(());
    }
    let now = match code {
        Some(code) => code,
        None if label.is_empty() => "(unknown)",
        None => label,
    };
    Err(format!("the source {now} may not be changed"))
}

/// Set the start source `old` of the driver list `list` back. `start_volume` is the volume at the
/// start of the check. [`back_rule`] decides: nothing is sent if the receiver has `old`; the check
/// selects `old` with [`restore_source`]; or the owner is asked with [`owner_source_back`], also
/// if the rule could not be read or the select failed. A failed set back of the check is a
/// failure.
async fn source_back(
    ctx: &mut Ctx,
    old: &str,
    list: &[String],
    start_volume: f64,
) -> Result<SetBack, Fail> {
    let (why, failed) = match back_rule(ctx, old, list, start_volume).await {
        Ok(Back::Same) => {
            return Ok(SetBack::note(
                "the receiver has the start source".to_owned(),
            ));
        }
        Ok(Back::Select) => match restore_source(ctx, old, 4).await {
            Ok(tries) => {
                return Ok(SetBack::note(format!(
                    "source set back to {old} in {tries} tries"
                )));
            }
            Err(e) => (format!("the set back failed: {e}"), Some(e.msg)),
        },
        Ok(Back::Owner(why)) => (why, None),
        Err(e) => (
            format!("the check could not read the state: {e}"),
            Some(e.msg),
        ),
    };
    let owner = owner_source_back(ctx, old, &why).await;
    match (failed, owner) {
        (None, owner) => owner,
        // The note of a skip has `why`, which has the error.
        (Some(_), Ok(back)) if back.skipped => Err(Fail::new(format!(
            "the check could not set the source back; {}",
            back.note
        ))),
        (Some(msg), Ok(back)) => Err(Fail::new(format!(
            "the check could not set the source back: {msg}; {}",
            back.note
        ))),
        (Some(msg), Err(e)) => Err(Fail::new(format!(
            "the check could not set the source back: {msg}; {e}"
        ))),
    }
}

/// Ask the owner to set the start source `old` back, because the check does not: `why`. The
/// answer s + Enter leaves the source as it is: the set back is skipped. Fails if the owner says
/// that it is set back but the driver does not show `old` within [`OWNER_BACK_SECS`], or if the
/// input ended.
async fn owner_source_back(ctx: &mut Ctx, old: &str, why: &str) -> Result<SetBack, Fail> {
    let mp = ctx.mp();
    let target = if old.is_empty() {
        "the source it had before the check".to_owned()
    } else {
        format!("{old:?}")
    };
    let text =
        format!("Set the source of the receiver back to {target}. The check does not: {why}.");
    let answer = ask(
        ctx,
        Ask::Action(
            "it is set back",
            "leave the source as it is, the check or the part skips if nothing failed",
        ),
        &text,
    )
    .await?;
    match answer {
        // Without a start source the check cannot know which source the owner selected.
        Answer::Done if old.is_empty() => Ok(SetBack::note(format!(
            "the owner set the source back, the receiver has {:?}",
            source_text(ctx)
        ))),
        Answer::Done => {
            ctx.client
                .wait_text(&mp, "source", OWNER_BACK_SECS, old)
                .await
                .map_err(|e| Fail::new(format!("the owner said it is set back, but: {e}")))?;
            Ok(SetBack::note(format!(
                "the owner set the source back to {old}"
            )))
        }
        Answer::Skip => Ok(SetBack {
            note: format!(
                "source not set back to {target}: {why}; the owner left it at {:?}",
                source_text(ctx)
            ),
            skipped: true,
        }),
        _ => Err(Fail::new(format!(
            "the source is not set back to {target}: {why}"
        ))),
    }
}

/// Prefer a plain mode that is not the current one.
fn pick_sound_mode(list: &[String], current: &str) -> Option<String> {
    let different = |m: &&String| !m.eq_ignore_ascii_case(current);
    ["stereo", "movie", "music"]
        .iter()
        .find_map(|p| {
            list.iter()
                .filter(different)
                .find(|m| m.to_lowercase().contains(p))
        })
        .or_else(|| list.iter().find(different))
        .cloned()
}

async fn select_sound(ctx: &mut Ctx, mode: &str) -> Result<(), Fail> {
    let mp = ctx.mp();
    ctx.client
        .command_ok(
            "media_player",
            &mp,
            "select_sound_mode",
            json!({ "mode": mode }),
        )
        .await
}

pub async fn sound_mode(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let old = ctx.client.text(&mp, "sound_mode");
    let list = strings(
        ctx.client
            .attr(&mp, "sound_mode_list")
            .unwrap_or(&Value::Null),
    );
    ensure!(!list.is_empty(), "the sound mode list is empty");
    if old.is_empty() {
        return Err(Fail::skip(
            "no sound mode is shown, it could not be set back",
        ));
    }
    if !list.iter().any(|m| m.eq_ignore_ascii_case(&old)) {
        return Err(Fail::skip(format!(
            "the sound mode {old} is not in the list, it could not be set back"
        )));
    }
    let new = pick_sound_mode(&list, &old).ok_or_else(|| Fail::skip("no other sound mode"))?;
    let result = async {
        select_sound(ctx, &new).await?;
        // The receiver can report the mode in other letter case than the list.
        ctx.client
            .wait_attr(&mp, "sound_mode", 10, |v| {
                v.as_str().is_some_and(|m| m.eq_ignore_ascii_case(&new))
            })
            .await?;
        let shown = ctx.client.text(&mp, "sound_mode");
        ensure!(
            ctx.client.text(&ctx.sensor("sound_mode"), "value") != "--",
            "the sound mode sensor is empty"
        );
        verify_all(ctx, 6).await?;
        Ok(shown)
    }
    .await;
    let undo = restore_sound(ctx, &old, &list, result.is_err()).await;
    let shown = first_error(result, undo)?;
    verify_all(ctx, 6).await?;
    Ok(format!("{old} -> {new} (receiver: {shown}), restored"))
}

/// Select the old mode again. Without `force`, nothing is sent if the old mode is shown. The
/// receiver can report a name that is not in the list.
async fn restore_sound(ctx: &mut Ctx, old: &str, list: &[String], force: bool) -> Result<(), Fail> {
    let mp = ctx.mp();
    let is_old = |v: &Value| v.as_str().is_some_and(|m| m.eq_ignore_ascii_case(old));
    if old.is_empty() {
        return Err(Fail::new("no sound mode was shown before"));
    }
    ctx.client.listen_ms(300).await?;
    if !force && ctx.client.attr(&mp, "sound_mode").is_some_and(is_old) {
        return Ok(());
    }
    let Some(name) = list.iter().find(|m| m.eq_ignore_ascii_case(old)) else {
        return Err(Fail::new(format!(
            "the old mode {old} is not in the list {list:?}"
        )));
    };
    select_sound(ctx, name).await?;
    ctx.client
        .wait_attr(&mp, "sound_mode", 10, is_old)
        .await
        .map(|_| ())
        .map_err(|e| Fail::new(format!("sound mode {old}: {e}")))
}

pub async fn volume(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    // The start volume comes from the receiver: the driver can show an old, higher volume.
    let Some(start) = receiver_volume(ctx).await? else {
        return Err(Fail::skip(
            "no second connection: the start volume cannot be read from the receiver",
        ));
    };
    let shown = driver_volume(ctx).await?;
    if shown != Some(start) {
        return Err(Fail::skip(format!(
            "the driver shows the volume {shown:?}, the receiver {start}: the driver value is old"
        )));
    }
    if start > MAX_VOLUME {
        return Err(Fail::skip(format!(
            "the volume {start} is above the limit {MAX_VOLUME}"
        )));
    }
    if start < 3.0 || start.fract() != 0.0 {
        return Err(Fail::skip(format!(
            "the volume {start} is too low or not a whole number"
        )));
    }
    let result = async {
        // Down first: the volume never goes above the start value.
        set_volume(ctx, start - VOLUME_STEP_DOWN).await?;
        ctx.client
            .wait_attr(&mp, "volume", 6, |v| {
                v.as_f64() == Some(start - VOLUME_STEP_DOWN)
            })
            .await?;
        let sensor = ctx.sensor("volume_db");
        ctx.client
            .wait_attr(&sensor, "value", 6, |v| {
                v.as_f64() == Some(start - VOLUME_STEP_DOWN - 80.0)
            })
            .await?;
        ctx.client
            .command_ok("media_player", &mp, "volume_up", json!({}))
            .await?;
        let up = ctx
            .client
            .wait_attr(&mp, "volume", 6, |v| {
                v.as_f64().is_some_and(|x| x > start - VOLUME_STEP_DOWN)
            })
            .await?;
        let up = up.as_f64().unwrap_or(start);
        ensure!(
            up <= start,
            "volume_up went to {up}, above the start value {start}"
        );
        Ok(format!("volume {start}, up step gave {up}, restored"))
    }
    .await;
    let undo = volume_back(ctx, start).await;
    first_error(result, undo)
}

/// The lowest volume that the volume check sets, below its start volume.
const VOLUME_STEP_DOWN: f64 = 2.0;

/// The volume that the volume check sends to set the volume back: `start` if `fresh` is not
/// `start` and not below the lowest volume that the check sets. A lower volume was set by someone
/// else: it stays.
fn volume_back_target(fresh: f64, start: f64) -> Option<f64> {
    (fresh != start && fresh >= start - VOLUME_STEP_DOWN).then_some(start)
}

/// Set the volume back to `start`, the volume of the receiver before the change, by the rule of
/// [`volume_back_target`]. `start` is not above [`MAX_VOLUME`]: the volume check skips a higher
/// start volume.
async fn volume_back(ctx: &mut Ctx, start: f64) -> Result<(), Fail> {
    let mp = ctx.mp();
    let fresh = receiver_volume(ctx)
        .await?
        .ok_or_else(|| Fail::new("no second connection"))?;
    let Some(start) = volume_back_target(fresh, start) else {
        return Ok(());
    };
    let sent = set_volume(ctx, start).await?;
    ctx.client
        .wait_attr(&mp, "volume", 6, |v| v.as_f64() == Some(sent))
        .await?;
    Ok(())
}

pub async fn power(ctx: &mut Ctx) -> Check {
    if ctx.opt.no_power {
        return Err(Fail::skip("--no-power"));
    }
    if ctx.oracle.is_none() {
        return Err(Fail::skip(
            "no second connection: the volume after a power on cannot be read from the receiver",
        ));
    }
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    ctx.client.listen_ms(300).await?;
    let Some(muted) = ctx.client.attr(&mp, "muted").and_then(Value::as_bool) else {
        return Err(Fail::skip("the mute state is not known"));
    };
    let before = baseline_volume(ctx).await?;
    let result = async {
        // Mute first: a power on can start with another volume.
        if !muted {
            ctx.client
                .command_ok("media_player", &mp, "mute", json!({}))
                .await?;
            ctx.client
                .wait_attr(&mp, "muted", 6, |v| *v == json!(true))
                .await?;
        }
        ctx.client
            .command_ok("media_player", &mp, "off", json!({}))
            .await?;
        ctx.client.wait_text(&mp, "state", 15, "OFF").await?;
        ctx.client.listen(1).await?;
        let (volume_sensor, eco) = (ctx.sensor("volume_db"), ctx.select("eco_mode"));
        ensure!(
            ctx.client.text(&volume_sensor, "value") == "--",
            "volume sensor while off"
        );
        ensure!(
            ctx.client.text(&eco, "current_option") == "--",
            "eco select while off"
        );
        ensure!(
            ctx.client.text(&mp, "source").is_empty(),
            "source while off"
        );
        let start = Instant::now();
        let on = power_on(ctx).await;
        let took = start.elapsed().as_millis();
        on?;
        ctx.client
            .wait_attr(&volume_sensor, "value", 10, |v| v.as_f64().is_some())
            .await?;
        Ok(took)
    }
    .await;
    let undo = power_back(ctx, muted, before, result.is_err()).await;
    let took = first_error(result, undo)?;
    verify_fresh(ctx).await?;
    Ok(format!("off, then on after {took} ms, values are back"))
}

/// Switch the receiver on with [`on_again`] if the driver or the second connection says that it
/// is off, else lower the volume with [`lower_volume`]. Then set the mute state back to `muted`.
/// `before` is the volume before the check. `force` is passed to [`set_muted`].
async fn power_back(
    ctx: &mut Ctx,
    muted: bool,
    before: Option<f64>,
    force: bool,
) -> Result<(), Fail> {
    ctx.client.listen_ms(300).await?;
    let volume = if !is_on(&ctx.client.text(&ctx.mp(), "state")) || receiver_off(ctx) {
        on_again(ctx, before).await
    } else {
        lower_volume(ctx, before).await
    };
    // The mute step runs also after an error: an unmute goes through `unmute_safely`.
    let mute = set_muted(ctx, muted, before, force).await;
    join_errors(volume, mute)
}

/// The second connection says that the main zone is not on. False without that connection.
fn receiver_off(ctx: &Ctx) -> bool {
    ctx.oracle
        .as_ref()
        .is_some_and(|oracle| oracle.state().zone(Zone::Main).power != Some(Power::On))
}

/// Switch the receiver on with [`power_on`], then lower the volume with [`lower_volume`]. The
/// volume is lowered also after an error of [`power_on`], except after a skip.
async fn on_again(ctx: &mut Ctx, before: Option<f64>) -> Result<(), Fail> {
    let on = power_on(ctx).await.map(|_| ());
    if on.as_ref().is_err_and(|e| e.skip) {
        return on;
    }
    let lowered = lower_volume(ctx, before).await;
    join_errors(on, lowered)
}

// ---------------------------------------------------------------- setup changes

/// Change a setup option of the receiver and wait until the setup is done.
async fn configure(ctx: &mut Ctx, address: &str, telnet: Option<bool>) -> Result<(), Fail> {
    let values = SetupValues {
        telnet,
        ..SetupValues::default()
    };
    configure_with(ctx, address, values).await.map(drop)
}

/// Setup fields for [`configure_with`]. `None` keeps the value that the setup page shows.
#[derive(Clone, Copy, Default)]
struct SetupValues {
    /// Telnet, else HTTP.
    telnet: Option<bool>,
    signal_poll_secs: Option<u16>,
    show_all_inputs: Option<bool>,
}

// The signal times are the same as in `config_store.rs` and `setup.rs` of the driver.

/// Default signal time of the driver in seconds.
const DEFAULT_SIGNAL_SECS: u16 = 5;
/// Lowest signal time of the driver in seconds, other than 0.
const MIN_SIGNAL_SECS: u16 = 3;
/// Highest signal time of the setup page in seconds.
const MAX_SIGNAL_SECS: u16 = 3600;

/// The signal time that the setup page shows for the receiver entry of `config.json`: no value is
/// [`DEFAULT_SIGNAL_SECS`], 0 stays 0, other values are moved into the range of the page.
fn signal_secs(device: &Value) -> u16 {
    match device["signal_poll_secs"].as_u64() {
        None => DEFAULT_SIGNAL_SECS,
        Some(0) => 0,
        Some(secs) => u16::try_from(secs)
            .unwrap_or(u16::MAX)
            .clamp(MIN_SIGNAL_SECS, MAX_SIGNAL_SECS),
    }
}

/// Change setup fields of the receiver and wait until the setup is done. A value of `values`
/// without a field on the setup page is not sent: the result lists the ids of these fields.
async fn configure_with(
    ctx: &mut Ctx,
    address: &str,
    values: SetupValues,
) -> Result<Vec<&'static str>, Fail> {
    let key = ctx.key.clone();
    ctx.client
        .request_ok(
            "setup_driver",
            json!({ "reconfigure": true, "setup_data": {} }),
        )
        .await?;
    ctx.client.next_page(10).await?;
    ctx.client
        .answer(json!({ "choice": key, "action": "configure" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    let ids = field_ids(&page);
    let has = |id: &str| ids.iter().any(|f| f == id);
    ensure!(has("connection_mode"), "configure page: {page}");
    // The Remote sends all values as text.
    let mut answer = json!({ "address": address });
    if let Some(telnet) = values.telnet {
        answer["connection_mode"] = if telnet { "use_telnet" } else { "use_http" }.into();
    }
    let fields = [
        (
            "signal_poll_secs",
            values.signal_poll_secs.map(|s| s.to_string()),
        ),
        (
            "show_all_inputs",
            values.show_all_inputs.map(|on| on.to_string()),
        ),
    ];
    let mut missing = Vec::new();
    for (id, value) in fields {
        match value {
            Some(value) if has(id) => answer[id] = value.into(),
            Some(_) => missing.push(id),
            None => {}
        }
    }
    ctx.client.answer(answer).await?;
    ctx.client.setup_done(20).await?;
    Ok(missing)
}

/// A Remote that reads the entities again after a mode change.
async fn refresh_remote(ctx: &mut Ctx) -> Result<usize, Fail> {
    ctx.reconnect().await?;
    ctx.client.forget_states();
    subscribe_all(ctx).await
}

/// Fails if `after` has other entity ids than `before`. Lists the missing and the new ids.
fn same_ids(before: &BTreeSet<String>, after: &BTreeSet<String>, when: &str) -> Result<(), Fail> {
    let missing: Vec<_> = before.difference(after).collect();
    let extra: Vec<_> = after.difference(before).collect();
    ensure!(
        missing.is_empty() && extra.is_empty(),
        "entities {when}: missing {missing:?}, new {extra:?}"
    );
    Ok(())
}

/// Driver log line for a setup flow that is aborted while it waits for user input.
const ABORT_LINE: &str = "State machine transition: WaitSetupUserData -> RequireSetup";

/// Abort the setup flow like a user on the Remote. Fails if a setup event is waiting or comes in
/// the next 500 ms.
async fn abort_driver_setup(ctx: &mut Ctx) -> Result<(), Fail> {
    ctx.client
        .send(json!({
            "kind": "event",
            "msg": "abort_driver_setup",
            "cat": "DEVICE",
            "msg_data": { "error": "ABORTED" },
        }))
        .await?;
    ctx.client.listen_ms(500).await?;
    let events = ctx.client.clear_setup();
    ensure!(events == 0, "{events} setup events after the abort");
    Ok(())
}

/// The user aborts a setup flow. The configuration and the entities must stay, the media player
/// must be available after it, and a new setup flow must start.
pub async fn abort_setup(ctx: &mut Ctx) -> Check {
    let config = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    let ids = entity_ids(ctx).await?;
    let from = ctx.driver.log_size().await;
    for round in 1..=2 {
        ctx.client.clear_setup();
        ctx.client
            .request_ok(
                "setup_driver",
                json!({ "reconfigure": true, "setup_data": {} }),
            )
            .await?;
        // The setup flow is open now: abort it also when the page is wrong.
        let page = match ctx.client.next_page(10).await {
            Ok(page) if field_ids(&page).iter().any(|f| f == "choice") => Ok(()),
            Ok(page) => Err(Fail::new(format!("first page of setup {round}: {page}"))),
            Err(e) => Err(e),
        };
        let abort = abort_driver_setup(ctx)
            .await
            .map_err(|e| Fail::new(format!("abort: {e}")));
        join_errors(page, abort)?;
    }
    let aborts = ctx.driver.log_from(from).await.matches(ABORT_LINE).count();
    ensure!(
        aborts == 2,
        "the driver logged {aborts} of 2 aborted setup flows (the info log of the driver is needed)"
    );
    ctx.client.refresh_states().await?;
    let state = wait_available(ctx, 10).await?;
    let after = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    ensure!(after == config, "config.json changed after the abort");
    same_ids(&ids, &entity_ids(ctx).await?, "after the abort")?;
    Ok(format!("state {state} after two aborted setups"))
}

pub async fn http_mode(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    // After a connect, the driver can show the sound mode a few seconds later: it waits for the
    // list of the receiver.
    let start = Instant::now();
    let old_sound = ctx
        .client
        .wait_attr(&mp, "sound_mode", 10, |v| {
            v.as_str().is_some_and(|m| !m.is_empty())
        })
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let waited = start.elapsed().as_millis();
    let mut changes = HttpChanges::default();
    let result = http_checks(ctx, &old_sound, &mut changes).await;
    // Always go back to telnet and set the signal time, the source and the sound mode back, also
    // after a failure.
    let force = result.is_err();
    let address = ctx.opt.address.clone();
    // A note if the old sound mode did not come back and the receiver is in a third mode: neither
    // the mode before nor the mode set in HTTP mode (see [`other_sound`]).
    let mut kept = None;
    let undo = async {
        let telnet = async {
            let values = SetupValues {
                telnet: Some(true),
                signal_poll_secs: changes.signal_secs,
                ..SetupValues::default()
            };
            let missing = configure_with(ctx, &address, values).await?;
            refresh_remote(ctx).await?;
            wait_available(ctx, 20).await?;
            // The signal time is not set back without its field.
            ensure!(
                missing.is_empty(),
                "the setup page has no {missing:?} field"
            );
            Ok(())
        };
        let telnet = telnet
            .await
            .map(|_| ())
            .map_err(|e| Fail::new(format!("back to telnet: {e}")));
        // The source is set back also if the switch to telnet failed. The shown source is not
        // trusted then.
        let source = match &changes.source {
            Some(source) => {
                let source = source.clone();
                async {
                    ctx.client.listen_ms(300).await?;
                    if telnet.is_err() || ctx.client.text(&mp, "source") != source {
                        restore_source(ctx, &source, 4).await?;
                    }
                    Ok(())
                }
                .await
            }
            None => Ok(()),
        };
        let failed = telnet.is_err();
        let errors = join_errors(telnet, source);
        // Without telnet the list of sound modes is not known. Without a mode shown before,
        // `http_checks` does not change the sound mode.
        if failed || old_sound.is_empty() {
            return errors;
        }
        // The list of the receiver comes over telnet after the connect. Until then the list has
        // the groups of HTTP mode, which do not have every mode.
        let has_old = |v: &Value| {
            v.as_array().is_some_and(|modes| {
                modes.iter().any(|m| {
                    m.as_str()
                        .is_some_and(|m| m.eq_ignore_ascii_case(&old_sound))
                })
            })
        };
        let list = ctx
            .client
            .wait_attr(&mp, "sound_mode_list", 10, has_old)
            .await
            .ok()
            .or_else(|| ctx.client.attr(&mp, "sound_mode_list").cloned())
            .unwrap_or(Value::Null);
        let sound = match restore_sound(ctx, &old_sound, &strings(&list), force).await {
            Err(e) => {
                let now = ctx.client.text(&mp, "sound_mode");
                let receiver = receiver_sound(ctx);
                if !now.is_empty() && other_sound(&receiver, changes.sound.as_ref()) {
                    kept = Some(format!(
                        "not restored, the driver shows {now}, the receiver {receiver} ({e})"
                    ));
                    Ok(())
                } else {
                    Err(e)
                }
            }
            ok => ok,
        };
        join_errors(errors, sound)
    }
    .await;
    let note = first_error(result, undo)?;
    let shown = match (old_sound.is_empty(), kept) {
        (true, _) => "not shown".to_owned(),
        (false, Some(kept)) => format!("shown after {waited} ms, {kept}"),
        (false, None) => format!("shown after {waited} ms, restored"),
    };
    Ok(format!("{note} (sound mode with telnet {shown})"))
}

/// The receiver has a mode `now` that is neither of `modes`: its modes before and after the change
/// in HTTP mode. All are the raw modes of the second connection. The input signal can change, for
/// example with a source change, and the receiver then picks a mode that fits the signal: the old
/// mode may not come back. False without `modes`.
fn other_sound(now: &str, modes: Option<&(String, String)>) -> bool {
    modes.is_some_and(|(before, after)| {
        !now.is_empty() && !now.eq_ignore_ascii_case(before) && !now.eq_ignore_ascii_case(after)
    })
}

/// The raw sound mode of the receiver from the second connection. Empty without that connection or
/// a mode.
fn receiver_sound(ctx: &Ctx) -> String {
    ctx.oracle
        .as_ref()
        .and_then(|oracle| oracle.state().zone(Zone::Main).sound_mode.clone())
        .unwrap_or_default()
}

/// What [`http_checks`] changed, for [`http_mode`] to set back.
#[derive(Default)]
struct HttpChanges {
    /// The source at the start. Set before a source change.
    source: Option<String>,
    /// The signal time at the start, as the setup page shows it (see [`signal_secs`]). Set before a
    /// change of the signal time.
    signal_secs: Option<u16>,
    /// The raw sound modes of the receiver (second connection) before and after the change in
    /// HTTP mode.
    sound: Option<(String, String)>,
}

/// The checks in HTTP mode. The sound mode is changed only if `old_sound`, the mode shown with
/// telnet, is known: [`http_mode`] sets it back. `changes` gets the values at the start before
/// they are changed, for [`http_mode`] to set them back with the switch back to telnet, and the
/// sound modes of the receiver before and after the change.
async fn http_checks(ctx: &mut Ctx, old_sound: &str, changes: &mut HttpChanges) -> Check {
    let mp = ctx.mp();
    let before = entity_ids(ctx).await?.len();
    let address = ctx.opt.address.clone();
    configure(ctx, &address, Some(false)).await?;
    let count = refresh_remote(ctx).await?;
    ensure!(
        count < before,
        "{count} entities in HTTP mode, {before} with telnet"
    );
    let (expected, info_note) = check_http_entities(ctx).await?;
    wait_available(ctx, 20).await?;
    let count = |prefix: &str| expected.iter().filter(|i| i.starts_with(prefix)).count();
    let mut note = vec![format!(
        "{} selects, {} sensors, {} switches, {} numbers",
        count("select."),
        count("sensor."),
        count("switch."),
        count("number."),
    )];
    note.extend(info_note);
    note.push(signal_entities(ctx, &expected, changes).await?);

    // Receivers read with the status pages have no dimmer over HTTP.
    if expected.contains("select.dimmer") {
        let old = ctx.client.text(&ctx.select("dimmer"), "current_option");
        let result = async {
            let start = Instant::now();
            let (old, new) = change_select(ctx, "dimmer", 12).await?;
            let took = start.elapsed().as_millis();
            ctx.client
                .wait_text(&ctx.sensor("dimmer"), "value", 12, &new)
                .await?;
            set_option(ctx, "dimmer", &old, 12).await?;
            Ok(took)
        }
        .await;
        let back = select_back(ctx, "dimmer", &old, 12, result.is_err()).await;
        let took = first_error(result, back)?;
        note.push(format!("dimmer {took} ms"));
    } else {
        note.push("no dimmer over HTTP".to_owned());
    }
    note.push(http_bass(ctx, &expected).await?);

    let old = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    ensure!(!list.is_empty(), "no source list over HTTP");
    let picked = read_sources(ctx)
        .await
        .and_then(|sources| other_source(&source_rule(ctx, &sources), &old, &list));
    let picked = match picked {
        Ok(new) => too_loud(ctx).await?.map_or(Ok(new), Err),
        Err(why) => Err(why),
    };
    match picked {
        Ok(new) => {
            changes.source = Some(old.clone());
            let start = Instant::now();
            let result = async {
                ctx.client
                    .command_ok(
                        "media_player",
                        &mp,
                        "select_source",
                        json!({ "source": new }),
                    )
                    .await?;
                ctx.client.wait_text(&mp, "source", 12, &new).await
            }
            .await;
            let took = start.elapsed().as_millis();
            let back = restore_source(ctx, &old, 12).await.map(|_| ());
            first_error(result, back)?;
            note.push(format!("source {took} ms"));
        }
        Err(why) => note.push(format!("source not changed: {why}")),
    }

    if old_sound.is_empty() {
        note.push("sound mode not changed: no mode was shown with telnet".to_owned());
        return Ok(note.join(", "));
    }
    let old = ctx.client.text(&mp, "sound_mode");
    let groups = strings(
        ctx.client
            .attr(&mp, "sound_mode_list")
            .unwrap_or(&Value::Null),
    );
    ensure!(
        groups.iter().any(|g| g == "STEREO"),
        "sound mode groups: {groups:?}"
    );
    let group = if old.eq_ignore_ascii_case("stereo") {
        "MOVIE"
    } else {
        "STEREO"
    };
    let receiver_before = receiver_sound(ctx);
    let start = Instant::now();
    select_sound(ctx, group).await?;
    ctx.client
        .wait_attr(&mp, "sound_mode", 15, |v| {
            v.as_str().is_some_and(|m| !m.is_empty() && m != old)
        })
        .await?;
    // The second connection can get the new mode later than the driver. Without a mode before the
    // change, or without a change, `sound` stays `None`, so a failed set back fails.
    let deadline = Instant::now() + Duration::from_secs(5);
    let sound = if receiver_before.is_empty() {
        None
    } else {
        loop {
            let now = receiver_sound(ctx);
            if !now.is_empty() && !now.eq_ignore_ascii_case(&receiver_before) {
                break Some(now);
            }
            if Instant::now() >= deadline {
                break None;
            }
            ctx.client.listen_ms(200).await?;
        }
    };
    changes.sound = sound.map(|after| (receiver_before, after));
    note.push(format!("sound mode {} ms", start.elapsed().as_millis()));
    note.push(format!("sound mode was {old} in HTTP mode"));
    Ok(note.join(", "))
}

/// Check that the driver offers the entities of HTTP mode that [`http_entities`] expects for
/// `config.json` as it is now. Returns the expected ids without the key and the note of
/// [`http_entities`].
async fn check_http_entities(
    ctx: &mut Ctx,
) -> Result<(BTreeSet<&'static str>, Option<String>), Fail> {
    let list = entity_ids(ctx).await?;
    let config = read_config(ctx).await?;
    let (expected, info_note) = http_entities(config_device(ctx, &config)?);
    let suffix = format!(".{}", ctx.key);
    let shown: BTreeSet<&str> = list
        .iter()
        .filter_map(|id| id.strip_suffix(suffix.as_str()))
        .filter(|id| id.contains('.'))
        .collect();
    let missing: Vec<_> = expected.difference(&shown).collect();
    let unexpected: Vec<_> = shown.difference(&expected).collect();
    ensure!(
        missing.is_empty() && unexpected.is_empty(),
        "HTTP mode entities missing: {missing:?}, not expected: {unexpected:?}"
    );
    Ok((expected, info_note))
}

/// In HTTP mode, set the signal time to 0 and then to [`DEFAULT_SIGNAL_SECS`], and check the
/// entities after each change with [`check_http_entities`]. With 0 the driver does not read the
/// signal pages, so their sensors go away. `start` is the expected set before the changes.
/// `changes.signal_secs` gets the value at the start before the first change. It is `None` again
/// if the setup page has no signal field.
async fn signal_entities(
    ctx: &mut Ctx,
    start: &BTreeSet<&'static str>,
    changes: &mut HttpChanges,
) -> Result<String, Fail> {
    let config = read_config(ctx).await?;
    let first = signal_secs(config_device(ctx, &config)?);
    let address = ctx.opt.address.clone();
    let mut note = format!("signal time {first} s");
    changes.signal_secs = Some(first);
    for secs in [0, DEFAULT_SIGNAL_SECS] {
        let values = SetupValues {
            telnet: Some(false),
            signal_poll_secs: Some(secs),
            ..SetupValues::default()
        };
        let missing = configure_with(ctx, &address, values)
            .await
            .map_err(|e| Fail::new(format!("signal time {secs} s: {e}")))?;
        if !missing.is_empty() {
            // The value was not sent.
            if secs == 0 {
                changes.signal_secs = None;
            }
            let _ = write!(
                note,
                ", {secs} s not sent: the setup page has no signal_poll_secs field"
            );
            return Ok(note);
        }
        let config = read_config(ctx).await?;
        let saved = signal_secs(config_device(ctx, &config)?);
        ensure!(
            saved == secs,
            "signal time {secs} s: config.json has {saved} s"
        );
        refresh_remote(ctx).await?;
        let (expected, _) = check_http_entities(ctx)
            .await
            .map_err(|e| Fail::new(format!("signal time {secs} s: {e}")))?;
        let gone: Vec<_> = start.difference(&expected).collect();
        let new: Vec<_> = expected.difference(start).collect();
        let _ = write!(
            note,
            ", {secs} s: {} entities, gone {gone:?}, new {new:?}",
            expected.len()
        );
    }
    wait_available(ctx, 20).await?;
    Ok(note)
}

/// Why the bass is not changed in HTTP mode, or `None` if it may be changed. `tone` is the state
/// of the tone control switch, `dirac_off` is from [`dirac_off`]. The tone control and the Dirac
/// filter are not changed by the check.
fn bass_skip(tone: &str, dirac_off: Option<bool>) -> Option<String> {
    if tone != "ON" {
        return Some(format!("the tone control is {tone:?}, not ON"));
    }
    match dirac_off {
        Some(true) => None,
        Some(false) => Some("a Dirac filter is on".to_owned()),
        None => Some("the Dirac filter is not known".to_owned()),
    }
}

/// True if the Dirac filter is off, `None` if it is not known. `receiver` is the filter of the
/// second connection, `select` the option of the Dirac filter select of the driver if it has one,
/// `supported` the Dirac setup option.
fn dirac_off(receiver: Option<DiracFilter>, select: Option<&str>, supported: bool) -> Option<bool> {
    match (receiver, select) {
        (Some(filter), _) => Some(filter == DiracFilter::Off),
        (None, Some("" | "--")) => None,
        (None, Some(option)) => Some(option == DiracFilter::Off.label()),
        (None, None) => (!supported).then_some(true),
    }
}

/// The bass level one `step` lower than `value`, or `None` if that is below `min`.
fn bass_lower(value: i64, step: i64, min: i64) -> Option<i64> {
    value.checked_sub(step).filter(|v| *v >= min)
}

/// In HTTP mode, lower the bass one step and set it back. The driver must show the new value in
/// 2 s. Only with the tone control on and the Dirac filter off. The note also has what MultEQ and
/// Dynamic EQ show. `expected` is the set of [`check_http_entities`].
async fn http_bass(ctx: &mut Ctx, expected: &BTreeSet<&'static str>) -> Result<String, Fail> {
    let key = ctx.key.clone();
    let shown = |kind: &str, attr: &str, ctx: &Ctx| {
        if expected.contains(kind) {
            ctx.client.text(&format!("{kind}.{key}"), attr)
        } else {
            "not offered".to_owned()
        }
    };
    // After a connect, the values can come a moment later. A missing value is reported below.
    let bass = format!("number.bass.{key}");
    let tone_id = format!("switch.tone_control.{key}");
    if expected.contains("number.bass") {
        let _ = ctx
            .client
            .wait_attr(&bass, "value", 10, Value::is_i64)
            .await;
    }
    if expected.contains("switch.tone_control") {
        let _ = ctx
            .client
            .wait_attr(&tone_id, "state", 10, |v| {
                v.as_str().is_some_and(|s| s == "ON" || s == "OFF")
            })
            .await;
    }
    let eq = format!(
        "MultEQ {:?}, Dynamic EQ {:?}",
        shown("select.multi_eq", "current_option", ctx),
        shown("switch.dynamic_eq", "state", ctx)
    );
    if !expected.contains("number.bass") {
        return Ok(format!("no bass over HTTP, {eq}"));
    }
    let tone = shown("switch.tone_control", "state", ctx);
    let select = expected.contains("select.dirac_filter").then(|| {
        ctx.client
            .text(&ctx.select("dirac_filter"), "current_option")
    });
    let config = read_config(ctx).await?;
    let supported = config_device(ctx, &config)?["is_dirac_supported"] == true;
    let off = dirac_off(oracle_dirac(ctx), select.as_deref(), supported);
    if let Some(why) = bass_skip(&tone, off) {
        return Ok(format!("bass not changed: {why}, {eq}"));
    }
    let options = entity_list(ctx)
        .await?
        .into_iter()
        .find(|e| e["entity_id"] == bass.as_str())
        .map(|mut e| e["options"].take())
        .unwrap_or_default();
    let step = options["step"]
        .as_f64()
        .map_or(1, |s| (s.round() as i64).max(1));
    let min = options["min"].as_f64().map_or(-6, |m| m.round() as i64);
    let Some(start) = ctx.client.attr(&bass, "value").and_then(Value::as_i64) else {
        return Ok(format!("bass has no value, {eq}"));
    };
    let Some(lower) = bass_lower(start, step, min) else {
        return Ok(format!("bass {start} dB is the lowest value, {eq}"));
    };
    let begin = Instant::now();
    let result = async {
        ctx.client
            .command_ok("number", &bass, "set_value", json!({ "value": lower }))
            .await?;
        ctx.client
            .wait_attr(&bass, "value", 2, |v| v.as_i64() == Some(lower))
            .await?;
        Ok(begin.elapsed().as_millis())
    }
    .await;
    let back = tone_back(ctx, &tone_id, None, &bass, Some(start), result.is_err()).await;
    let took = first_error(result, back)?;
    Ok(format!(
        "bass {start} -> {lower} dB in {took} ms, set back, {eq}"
    ))
}

/// The number of sources in the source list of the driver, read with `get_entity_states`. Reads
/// again until the media player is available with a source list and `done` takes the number, or
/// `secs` pass. Fails if the media player has no source list or is not available after 20 s more.
/// With `secs` 0 it returns the first number of a ready media player.
async fn source_count(
    ctx: &mut Ctx,
    secs: u64,
    done: impl Fn(usize) -> bool,
) -> Result<usize, Fail> {
    let mp = ctx.mp();
    let start = Instant::now();
    loop {
        ctx.client.refresh_states().await?;
        let count = ctx
            .client
            .attr(&mp, "source_list")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let state = ctx.client.text(&mp, "state");
        let ready = count > 0 && state != "UNAVAILABLE";
        let waited = start.elapsed();
        if ready && (done(count) || waited >= Duration::from_secs(secs)) {
            return Ok(count);
        }
        ensure!(
            waited < Duration::from_secs(secs + 20),
            "no source list: {count} sources, state {state}"
        );
        ctx.client.listen_ms(500).await?;
    }
}

/// Set the setup option "Show all sources" and check that `config.json` has it.
async fn set_show_all(ctx: &mut Ctx, on: bool) -> Result<(), Fail> {
    let address = ctx.opt.address.clone();
    let values = SetupValues {
        show_all_inputs: Some(on),
        ..SetupValues::default()
    };
    let missing = configure_with(ctx, &address, values).await?;
    ensure!(
        missing.is_empty(),
        "the setup page has no show_all_inputs field"
    );
    let config = read_config(ctx).await?;
    let saved = &config_device(ctx, &config)?["show_all_inputs"];
    ensure!(
        *saved == on,
        "show_all_inputs {on}: config.json has {saved}"
    );
    Ok(())
}

/// Time for the source list to change after a change of "Show all sources", in seconds.
const SHOW_ALL_SECS: u64 = 6;

/// Change the setup option "Show all sources" and check that the driver uses it without a restart:
/// with the option on, the source list has at least as many sources as with it off. The option is
/// set back to the value of `config.json`, also after a failure.
pub async fn configure_options(ctx: &mut Ctx) -> Check {
    let config = read_config(ctx).await?;
    let start = config_device(ctx, &config)?["show_all_inputs"] == true;
    let before = source_count(ctx, 0, |_| true).await?;
    let result = async {
        set_show_all(ctx, !start).await?;
        let after = source_count(ctx, SHOW_ALL_SECS, |n| n != before).await?;
        let (on, off) = if start {
            (before, after)
        } else {
            (after, before)
        };
        ensure!(
            on >= off,
            "{on} sources with all sources shown, {off} without"
        );
        Ok(format!(
            "{on} sources with all sources shown, {off} without"
        ))
    }
    .await;
    // Always set back, also after a failure.
    let undo = async {
        set_show_all(ctx, start).await?;
        let back = source_count(ctx, SHOW_ALL_SECS, |n| n == before).await?;
        ensure!(
            back == before,
            "{back} sources after the set back, {before} at the start"
        );
        Ok(())
    }
    .await;
    let note = first_error(result, undo)?;
    Ok(format!("{note}, set back to {start}"))
}

pub async fn backup_restore(ctx: &mut Ctx) -> Check {
    let key = ctx.key.clone();
    let ids = entity_ids(ctx).await?;
    ctx.client
        .request_ok(
            "setup_driver",
            json!({ "reconfigure": true, "setup_data": {} }),
        )
        .await?;
    ctx.client.next_page(10).await?;
    ctx.client
        .answer(json!({ "choice": key, "action": "backup" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    let backup = field_value(&page, "backup_data")
        .filter(|b| !b.is_empty())
        .ok_or_else(|| Fail::new("no backup text"))?;
    ctx.client.answer(json!({})).await?;
    ctx.client.setup_done(10).await?;

    ctx.client
        .request_ok(
            "setup_driver",
            json!({ "reconfigure": true, "setup_data": {} }),
        )
        .await?;
    ctx.client.next_page(10).await?;
    ctx.client
        .answer(json!({ "choice": key, "action": "restore" }))
        .await?;
    let page = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&page).contains(&"restore_data".to_owned()),
        "restore page: {page}"
    );
    // A wrong text keeps the page and changes nothing.
    ctx.client
        .answer(json!({ "restore_data": "not a backup" }))
        .await?;
    ctx.client.next_page(10).await?;
    ctx.client.answer(json!({ "restore_data": backup })).await?;
    ctx.client.setup_done(20).await?;
    refresh_remote(ctx).await?;
    let after = entity_ids(ctx).await?;
    ensure!(after == ids, "entities differ after the restore");
    wait_available(ctx, 20).await?;
    Ok(format!("backup of {} characters restored", backup.len()))
}

/// Pings that [`ws_keepalive`] sends, one per second.
const KEEPALIVE_PINGS: u8 = 5;
/// Close code that [`ws_close`] sends.
const CLOSE_CODE: u16 = 4001;
/// Close reason that [`ws_close`] sends.
const CLOSE_REASON: &str = "bye";
/// Time without messages in [`ws_idle`]. The driver closes after 40 s without a message.
const IDLE_SECS: u64 = 45;

/// The driver answers the pings of a Remote. Another WebSocket connection pings every second for 5 s, and
/// each pong must come within 1 s.
pub async fn ws_keepalive(ctx: &mut Ctx) -> Check {
    const SECOND: Duration = Duration::from_secs(1);
    let mut client = Client::connect(ctx.driver.port).await?;
    let mut slowest = Duration::ZERO;
    for i in 0..KEEPALIVE_PINGS {
        let start = Instant::now();
        let took = client
            .ping(&[i], SECOND)
            .await
            .map_err(|e| Fail::new(format!("ping {}: {e}", i + 1)))?;
        slowest = slowest.max(took);
        client.listen_until(start + SECOND).await?;
    }
    Ok(format!(
        "{KEEPALIVE_PINGS} pings, slowest pong {} ms",
        slowest.as_millis()
    ))
}

/// A Remote that sends nothing stays connected: the driver pings every 20 s and closes after 40 s
/// without a message. Another WebSocket connection only reads for 45 s, so only its pongs (sent by the
/// library) keep the connection open. Then it must still get an answer to a request.
pub async fn ws_idle(ctx: &mut Ctx) -> Check {
    let mut client = Client::connect(ctx.driver.port).await?;
    // The main connection is read too, so it answers the pings of the driver and stays open.
    let (idle, main) = tokio::join!(client.listen(IDLE_SECS), ctx.client.listen(IDLE_SECS));
    idle.map_err(|e| Fail::new(format!("idle connection: {e}")))?;
    main.map_err(|e| Fail::new(format!("main connection: {e}")))?;
    client
        .request_ok("get_driver_version", Value::Null)
        .await
        .map_err(|e| Fail::new(format!("after {IDLE_SECS} s: {e}")))?;
    Ok(format!("open after {IDLE_SECS} s without messages"))
}

/// A Remote that closes the connection. Another WebSocket connection sends a close frame. The driver must
/// answer with the same code and reason, log the close and end the session.
pub async fn ws_close(ctx: &mut Ctx) -> Check {
    let client = Client::connect(ctx.driver.port).await?;
    // The driver logs lines of a session with the peer address as `[id]`.
    let id = format!("[{}]", client.local_addr());
    let mark = ctx.driver.log_size().await;
    let frame = client
        .close(CLOSE_CODE, CLOSE_REASON, Duration::from_secs(3))
        .await?;
    close_echo(frame.as_ref(), CLOSE_CODE, CLOSE_REASON).map_err(Fail::new)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (closed, stopped) = session_end(&log_after(ctx, mark).await, &id);
        if closed && stopped {
            return Ok(format!(
                "close {CLOSE_CODE} {CLOSE_REASON:?} answered, session {id} stopped"
            ));
        }
        ensure!(
            Instant::now() < deadline,
            "in 3 s the driver log has: close line {closed}, {id} stopped {stopped}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// Whether `log` has the close line and the stop line of the session `id` (`[ip:port]`). The
/// message of a line must start with `id`, after the `[time level target] ` part.
fn session_end(log: &str, id: &str) -> (bool, bool) {
    let (mut closed, mut stopped) = (false, false);
    let rests = log
        .lines()
        .filter_map(|l| l.split_once("] ")?.1.strip_prefix(id));
    for rest in rests {
        closed |= rest.starts_with(" Remote closed connection");
        stopped |= rest == " stopped";
    }
    (closed, stopped)
}

pub async fn unreachable(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let address = ctx.opt.address.clone();
    // 192.0.2.0/24 is for documentation: nothing answers.
    configure(ctx, "192.0.2.1", None).await?;
    let result = async {
        let down = ctx.client.wait_text(&mp, "state", 20, "UNAVAILABLE").await;
        if down.is_err() {
            return Ok("the driver found the receiver again by its id".to_owned());
        }
        let select = ctx.select("dimmer");
        let (response, took) = ctx
            .client
            .command(
                "select",
                &select,
                "select_option",
                json!({ "option": "Dim" }),
            )
            .await?;
        if response["code"] == 200 {
            // The driver can find the receiver by its id while the command waits. It saves the
            // address a moment after the answer. The receiver address can be a host name, and the
            // driver can save one of its IP addresses.
            let ips: Vec<IpAddr> = tokio::net::lookup_host((address.as_str(), 0))
                .await
                .map(|addrs| addrs.map(|a| a.ip()).collect())
                .unwrap_or_default();
            let mut found = false;
            for _ in 0..10 {
                let config = read_config(ctx).await?;
                if is_address(&config_device(ctx, &config)?["address"], &address, &ips) {
                    found = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            ensure!(found, "a command worked without a connection");
            return Ok(format!(
                "the driver found the receiver again by its id while the command waited {} ms",
                took.as_millis()
            ));
        }
        ensure!(
            took < Duration::from_secs(10),
            "the command took {} ms",
            took.as_millis()
        );
        Ok(format!(
            "unavailable, a command failed after {} ms",
            took.as_millis()
        ))
    }
    .await;
    configure(ctx, &address, None).await?;
    wait_available(ctx, 60).await?;
    result
}

/// The `address` field of a configuration is `address`, or one of `ips`, the IP addresses of
/// `address`.
fn is_address(field: &Value, address: &str, ips: &[IpAddr]) -> bool {
    field.as_str().is_some_and(|field| {
        field == address || field.parse::<IpAddr>().is_ok_and(|ip| ips.contains(&ip))
    })
}

// ---------------------------------------------------------------- configuration file

async fn read_config(ctx: &Ctx) -> Result<Value, Fail> {
    let text = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    serde_json::from_str(&text).map_err(|e| Fail::new(format!("config.json: {e}")))
}

/// Fields of the file of the Python integration. `signal_poll_secs` is new: it is not written
/// while it has no value.
const PYTHON_FIELDS: [&str; 13] = [
    "id",
    "name",
    "address",
    "support_sound_mode",
    "show_all_inputs",
    "use_telnet",
    "update_audyssey",
    "zone2",
    "zone3",
    "volume_step",
    "timeout",
    "is_denon",
    "is_dirac_supported",
];

/// The entry of the receiver in `config.json`.
fn config_device<'a>(ctx: &Ctx, config: &'a Value) -> Result<&'a Value, Fail> {
    config
        .as_array()
        .and_then(|a| a.iter().find(|d| d["id"] == ctx.key.as_str()))
        .ok_or_else(|| Fail::new("the receiver is not in config.json"))
}

/// Entities that have a value, with the query of the value and whether the Python integration
/// has them without telnet. The tuner, network favorite and tuner frequency entities have no
/// query: they come with telnet only.
const QUERIED: [(&str, Query, bool); 65] = {
    use Query as Q;
    [
        ("select.sound_mode", Q::SoundMode, true),
        ("select.input_source", Q::Source, true),
        ("select.reference_level_offset", Q::ReferenceLevel, true),
        ("select.dynamic_volume", Q::DynamicVolume, true),
        ("select.dimmer", Q::Dimmer, true),
        ("select.eco_mode", Q::Eco, true),
        ("select.monitor_output", Q::HdmiOutput, false),
        ("select.speaker_preset", Q::SpeakerPreset, false),
        ("select.picture_mode", Q::PictureMode, false),
        ("select.digital_codec", Q::DigitalCodec, false),
        ("select.dirac_filter", Q::DiracFilter, false),
        ("select.multi_eq", Q::MultEq, false),
        ("select.quick_select", Q::QuickSelect, false),
        ("select.bluetooth_output", Q::BluetoothTransmitter, false),
        ("sensor.volume_db", Q::Volume, true),
        ("sensor.volume_absolute", Q::Volume, true),
        ("sensor.sound_mode", Q::SoundMode, true),
        ("sensor.input_source", Q::Source, true),
        ("sensor.mute", Q::Mute, true),
        ("sensor.dimmer", Q::Dimmer, true),
        ("sensor.eco_mode", Q::Eco, true),
        ("sensor.sleep_timer", Q::Sleep, false),
        ("sensor.audio_delay", Q::AudioDelay, false),
        ("sensor.audio_signal", Q::AudioSignal, false),
        ("sensor.audio_sampling_rate", Q::SamplingRate, false),
        ("sensor.monitor_output", Q::HdmiOutput, false),
        ("sensor.video_hdmi_signal_in", Q::VideoInput, false),
        ("sensor.video_hdmi_signal_out", Q::VideoOutput, false),
        ("sensor.input_channels", Q::InputChannels, false),
        ("sensor.output_channels", Q::OutputChannels, false),
        ("sensor.max_resolution", Q::HdmiMaxResolution, false),
        ("sensor.hdr_input", Q::HdrFormat, false),
        ("sensor.hdr_output", Q::HdrFormat, false),
        ("sensor.pixel_depth_input", Q::PixelDepth, false),
        ("sensor.pixel_depth_output", Q::PixelDepth, false),
        ("sensor.max_frl_input", Q::MaxFrl, false),
        ("sensor.max_frl_output", Q::MaxFrl, false),
        ("sensor.colorspace_input", Q::ColorSpace, false),
        ("sensor.colorspace_output", Q::ColorSpace, false),
        ("sensor.picture_mode", Q::PictureMode, false),
        ("sensor.all_zone_stereo", Q::AllZoneStereo, false),
        ("sensor.digital_codec", Q::DigitalCodec, false),
        ("sensor.dirac_filter", Q::DiracFilter, false),
        ("sensor.pro_logic_mode", Q::ProLogicMode, false),
        ("sensor.surround_back_mode", Q::SurroundBackMode, false),
        ("sensor.front_height", Q::FrontHeight, false),
        ("sensor.dynamic_compression", Q::DynamicCompression, false),
        ("sensor.auto_flag_detect", Q::AutoFlagDetect, false),
        ("sensor.dialog_level_adjust", Q::DialogLevel, false),
        ("sensor.dialog_level", Q::DialogLevel, false),
        ("sensor.audyssey_dsx", Q::AudysseyDsx, false),
        ("sensor.stage_width", Q::StageWidth, false),
        ("sensor.stage_height", Q::StageHeight, false),
        ("sensor.height_gain", Q::HeightGain, false),
        ("sensor.insta_prevue", Q::InstaPrevue, false),
        ("sensor.bluetooth_level", Q::BluetoothLevel, false),
        ("sensor.sound_mode_category", Q::SoundModeCategory, false),
        ("switch.tone_control", Q::ToneControl, false),
        ("switch.dynamic_eq", Q::DynamicEq, false),
        ("switch.trigger_1", Q::Trigger, false),
        ("switch.trigger_2", Q::Trigger, false),
        ("switch.trigger_3", Q::Trigger, false),
        (
            "switch.bluetooth_transmitter",
            Q::BluetoothTransmitter,
            false,
        ),
        ("number.bass", Q::Bass, false),
        ("number.treble", Q::Treble, false),
    ]
};

/// The selects, sensors, switches and numbers in HTTP mode for the `info` of the receiver in
/// `config.json`, as ids without the key. With `info`, an entity comes if the receiver may have
/// the value and the HTTP poll (`Avr::update` and `Avr::refresh_signal_pages`, which also does the
/// read of `Avr::update_signal_and_settings`; with a signal time of 0 only
/// `Avr::update_signal_and_settings`) reads it. The sources in `missing` give no values.
/// Without `info`, the entities of the Python integration come. Dirac also needs the setup option,
/// and the quick select its first slot. An `info` that does not load counts as none, as in the
/// driver: the note says so.
fn http_entities(device: &Value) -> (BTreeSet<&'static str>, Option<String>) {
    use denon_marantz::{HttpReads, Info, MissingSources};
    let (info, note) = match serde_json::from_value::<Option<Info>>(device["info"].clone()) {
        Ok(info) => (info, None),
        Err(e) => (None, Some(format!("info in config.json not read: {e}"))),
    };
    let missing: MissingSources =
        serde_json::from_value(device["missing"].clone()).unwrap_or_default();
    // The reads of the HTTP mode, `POLL_READS` in the driver. A signal time of 0 does not read the
    // signal pages.
    let mut poll = HttpReads::UPDATE | HttpReads::SIGNAL_AND_SETTINGS;
    if device["signal_poll_secs"] != 0 {
        poll |= HttpReads::SIGNAL_PAGES;
    }
    let dirac = device["is_dirac_supported"] == true;
    let entities = QUERIED
        .iter()
        .filter(|(name, query, base)| {
            let setup = match *name {
                "select.dirac_filter" | "sensor.dirac_filter" => dirac,
                "select.quick_select" => info.as_ref().is_none_or(|i| i.has_quick_select(1)),
                _ => true,
            };
            let read = info.as_ref().map_or(*base, |info| {
                info.offers(*query) && info.http_reads_without(*query, missing).intersects(poll)
            });
            setup && read
        })
        .map(|(name, _, _)| *name)
        .collect();
    (entities, note)
}

pub async fn config_file(ctx: &mut Ctx) -> Check {
    let config = read_config(ctx).await?;
    let device = config_device(ctx, &config)?;
    let missing: Vec<_> = PYTHON_FIELDS
        .iter()
        .filter(|f| device.get(**f).is_none())
        .collect();
    ensure!(
        missing.is_empty(),
        "fields missing in config.json: {missing:?}"
    );
    // `mac` is missing on receivers that do not give it. `id_kind` is missing for the serial
    // number.
    let kind = ["receiver_type", "update_method", "serial"];
    let missing: Vec<_> = kind.iter().filter(|f| device.get(**f).is_none()).collect();
    ensure!(
        missing.is_empty(),
        "fields missing in config.json: {missing:?}"
    );
    let id_field = if device["id_kind"] == "mac" {
        "mac"
    } else {
        "serial"
    };
    ensure!(
        device[id_field] == device["id"],
        "id {} is not the {id_field} {}",
        device["id"],
        device[id_field]
    );
    Ok(format!(
        "id {} ({}), serial {}, mac {}, address {}, use_telnet {}, receiver_type {}, update_method {}, quick_select_count {}",
        device["id"],
        device["id_kind"],
        device["serial"],
        device["mac"],
        device["address"],
        device["use_telnet"],
        device["receiver_type"],
        device["update_method"],
        device["info"]["quick_select_count"]
    ))
}

/// After a start, the driver reads the web pages and the Sound group in the background: wait until
/// all entities show the values of the receiver.
async fn settle(ctx: &mut Ctx) -> Result<(), Fail> {
    wait_available(ctx, 20).await?;
    verify_all(ctx, 30)
        .await
        .map_err(|e| Fail::new(format!("30 s after the start: {e}")))
}

/// The configuration files that the driver moved away because it could not read them.
async fn bad_configs(ctx: &Ctx) -> Result<Vec<std::path::PathBuf>, Fail> {
    let mut entries = tokio::fs::read_dir(&ctx.driver.dir).await?;
    let mut found = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("config.json.bad-")
        {
            found.push(entry.path());
        }
    }
    Ok(found)
}

/// Start the driver again and connect a new Remote.
async fn restart(ctx: &mut Ctx) -> Result<(), Fail> {
    ctx.driver.start().await?;
    ctx.reconnect().await
}

/// Start the driver again without `RUST_LOG` and connect a new Remote.
async fn restart_plain(ctx: &mut Ctx) -> Result<(), Fail> {
    ctx.driver.start_with(false).await?;
    ctx.reconnect().await
}

/// The power that the media player must show after a start of the driver.
#[derive(Debug, PartialEq)]
enum ShownPower {
    /// An on state: `ON`, `PLAYING` or `PAUSED`.
    On,
    /// `OFF`.
    Off,
    /// Not known: no wait.
    Unknown,
}

/// What the media player must show for the power of the main zone from the second connection.
fn restart_wait(power: Option<Power>) -> ShownPower {
    match power {
        Some(Power::On) => ShownPower::On,
        Some(Power::Standby | Power::Off) => ShownPower::Off,
        _ => ShownPower::Unknown,
    }
}

/// Start the driver again with the same configuration file. Right after the start the driver must
/// offer the same entities, because the Remote reads them when it connects. If the second
/// connection knows the power of the main zone, the media player must show it, and with the main
/// zone on, the entities must then show the values of the receiver. If the start fails, the driver
/// is started once more for the later steps.
pub async fn driver_restart(ctx: &mut Ctx) -> Check {
    let ids = entity_ids(ctx).await?;
    ctx.driver.stop().await;
    let start = Instant::now();
    if let Err(e) = restart(ctx).await {
        // Start once more, else all later steps fail without a driver.
        let again = restart(ctx)
            .await
            .map_err(|e| Fail::new(format!("second start: {e}")));
        return first_error(Err(e), again);
    }
    same_ids(&ids, &entity_ids(ctx).await?, "after the restart")?;
    subscribe_all(ctx).await?;
    // `verify_all` compares nothing while the driver does not show the receiver as on: wait for
    // on first. If the second connection says that the main zone is in standby or off, the media
    // player must show OFF.
    let power = ctx
        .oracle
        .as_ref()
        .and_then(|oracle| oracle.state().zone(Zone::Main).power);
    let mp = ctx.mp();
    match restart_wait(power) {
        ShownPower::On => {
            ctx.client
                .wait_attr(&mp, "state", 20, |v| v.as_str().is_some_and(is_on))
                .await
                .map_err(|e| {
                    Fail::new(format!(
                        "the receiver is on, the driver does not show it: {e}"
                    ))
                })?;
        }
        ShownPower::Off => {
            ctx.client
                .wait_text(&mp, "state", 20, "OFF")
                .await
                .map_err(|e| {
                    Fail::new(format!(
                        "the receiver is off, the driver does not show it: {e}"
                    ))
                })?;
        }
        ShownPower::Unknown => {}
    }
    settle(ctx).await?;
    let compared = if ctx.oracle.is_none() || receiver_off(ctx) {
        ", values not compared"
    } else {
        ""
    };
    Ok(format!(
        "{} entities, settled {} ms after the start{compared}",
        ids.len(),
        start.elapsed().as_millis()
    ))
}

/// Settings file of the driver. It holds the debug log option.
const SETTINGS_FILE: &str = "settings.json";

/// Number of debug lines in `log`. The level is the second word of a log line, so a receiver
/// text in another line does not count.
fn debug_lines(log: &str) -> usize {
    log.lines()
        .filter(|l| l.starts_with('[') && l.split(' ').nth(1) == Some("DEBUG"))
        .count()
}

/// The log after byte `start`. Only that part is read.
async fn log_after(ctx: &Ctx, start: u64) -> String {
    ctx.driver.log_from(start).await
}

/// Turn the debug log option on or off with "Save settings" in the configuration mode.
async fn save_debug_log(ctx: &mut Ctx, on: bool) -> Result<(), Fail> {
    let key = ctx.key.clone();
    ctx.client.clear_setup();
    ctx.client
        .request_ok(
            "setup_driver",
            json!({ "reconfigure": true, "setup_data": {} }),
        )
        .await?;
    let page = ctx.client.next_page(10).await?;
    ensure!(
        field_ids(&page).iter().any(|f| f == "debug_log"),
        "configuration mode page without debug_log: {page}"
    );
    ctx.client
        .answer(json!({ "choice": key, "action": "settings", "debug_log": on.to_string() }))
        .await?;
    ctx.client.setup_done(20).await
}

/// Wait up to 2 s until the settings file holds `on` for the debug log. The driver writes the
/// file in another thread after the setup ends.
async fn wait_settings(ctx: &Ctx, on: bool) -> Result<(), Fail> {
    let settings = ctx.driver.dir.join(SETTINGS_FILE);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let saved = tokio::fs::read_to_string(&settings)
            .await
            .unwrap_or_default();
        let value = serde_json::from_str::<Value>(&saved)
            .ok()
            .and_then(|v| v["debug_log"].as_bool());
        if value == Some(on) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "{SETTINGS_FILE} after 2 s, debug log {on} expected: {saved}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

/// Start the driver with the debug log option off, turn it on with the setup, then off, then
/// start the driver with the option on. The driver starts without `RUST_LOG`, because `RUST_LOG`
/// overrides the option.
async fn debug_log_checks(ctx: &mut Ctx) -> Check {
    let settings = ctx.driver.dir.join(SETTINGS_FILE);
    ctx.driver.stop().await;
    tokio::fs::write(&settings, r#"{"debug_log":false}"#).await?;
    let start = ctx.driver.log_size().await;
    restart_plain(ctx).await?;
    entity_ids(ctx).await?;
    let log = log_after(ctx, start).await;
    ensure!(
        debug_lines(&log) == 0,
        "{} debug lines with the option off",
        debug_lines(&log)
    );

    let start = ctx.driver.log_size().await;
    save_debug_log(ctx, true).await?;
    entity_ids(ctx).await?;
    let log = log_after(ctx, start).await;
    ensure!(
        !log.contains("RUST_LOG is used instead"),
        "the driver uses RUST_LOG, but it was started without it"
    );
    let on = log
        .find("debug log on")
        .ok_or_else(|| Fail::new("no 'debug log on' line after the option was saved"))?;
    ensure!(
        debug_lines(&log[on..]) > 0,
        "no debug lines after the option was turned on"
    );
    wait_settings(ctx, true).await?;

    let start = ctx.driver.log_size().await;
    save_debug_log(ctx, false).await?;
    entity_ids(ctx).await?;
    let log = log_after(ctx, start).await;
    let off = log
        .find("debug log off")
        .ok_or_else(|| Fail::new("no 'debug log off' line after the option was saved"))?;
    let after = debug_lines(&log[off..]);
    ensure!(
        after == 0,
        "{after} debug lines after the option was turned off"
    );

    wait_settings(ctx, false).await?;

    save_debug_log(ctx, true).await?;
    wait_settings(ctx, true).await?;
    ctx.driver.stop().await;
    let start = ctx.driver.log_size().await;
    restart_plain(ctx).await?;
    entity_ids(ctx).await?;
    let log = log_after(ctx, start).await;
    ensure!(
        log.contains("Debug log is on"),
        "no 'Debug log is on' line after a start with the option on"
    );
    ensure!(
        debug_lines(&log) > 0,
        "no debug lines after a start with the option on"
    );
    Ok("option on, off and on after a start".to_owned())
}

/// The debug log option of the setup. The driver of the other checks gets `RUST_LOG`, so the
/// option has no effect there. At the end the settings file is set back and the driver is started
/// again with `RUST_LOG`.
pub async fn debug_log(ctx: &mut Ctx) -> Check {
    let settings = ctx.driver.dir.join(SETTINGS_FILE);
    let before = match tokio::fs::read(&settings).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let result = debug_log_checks(ctx).await;
    ctx.driver.stop().await;
    let file = match &before {
        Some(bytes) => tokio::fs::write(&settings, bytes).await.map_err(Fail::from),
        None => match tokio::fs::remove_file(&settings).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    };
    let started = async {
        // Start once more, else all later steps fail without a driver. A failed first start is
        // still a failure.
        let first = match restart(ctx).await {
            Err(e) => {
                restart(ctx)
                    .await
                    .map_err(|again| Fail::new(format!("{e}; second start: {again}")))?;
                Err(e)
            }
            Ok(()) => Ok(()),
        };
        let ready = async {
            subscribe_all(ctx).await?;
            wait_available(ctx, 20).await.map(|_| ())
        }
        .await;
        join_errors(first, ready)
    }
    .await;
    first_error(result, join_errors(file, started))
}

/// The file of the Python integration: only its fields, and the serial number is the id. The
/// driver must keep the serial number as the key and add the MAC address and the serial number.
pub async fn python_config(ctx: &mut Ctx) -> Check {
    let ids = entity_ids(ctx).await?;
    let good = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    let config = read_config(ctx).await?;
    let device = config_device(ctx, &config)?;
    let serial = device["serial"]
        .as_str()
        .ok_or_else(|| Fail::new("config.json has no serial number"))?
        .to_owned();
    let mac = device["mac"].clone();
    // The setup option "Receiver has a tuner" is not in the file of the Python integration. Without
    // it, the receiver decides: the tuner entities can come or go.
    let tuner_differs = has_tuner(device) != (device["has_tuner"] != false);
    let mut python = serde_json::Map::new();
    for field in PYTHON_FIELDS {
        if let Some(value) = device.get(field) {
            python.insert(field.to_owned(), value.clone());
        }
    }
    python.insert("id".to_owned(), json!(serial));
    // The same entities with the serial number as the key.
    let mac_key = std::mem::replace(&mut ctx.key, serial.clone());
    let expected: BTreeSet<String> = ids
        .iter()
        .map(|id| {
            id.strip_suffix(mac_key.as_str())
                .map_or_else(|| id.clone(), |kind| format!("{kind}{serial}"))
        })
        .collect();
    ctx.driver.stop().await;
    let result = async {
        tokio::fs::write(ctx.driver.config_file(), json!([python]).to_string()).await?;
        restart(ctx).await?;
        // Right after the start the receiver may not be read yet: the driver can offer more
        // entities.
        let before = entity_ids(ctx).await?;
        let missing: Vec<_> = expected.difference(&before).collect();
        ensure!(
            missing.is_empty(),
            "entities missing with the key {serial}: {missing:?}"
        );
        subscribe_all(ctx).await?;
        let state = wait_available(ctx, 20).await?;
        // The driver saves the ids off its main thread.
        let end = Instant::now() + Duration::from_secs(5);
        let saved = loop {
            let config = read_config(ctx).await?;
            let device = config_device(ctx, &config)?.clone();
            if device.get("mac").is_some() || Instant::now() > end {
                break device;
            }
            sleep(Duration::from_millis(100)).await;
        };
        ensure!(
            saved["mac"] == mac && saved["serial"] == serial.as_str(),
            "ids not added: serial {}, mac {}",
            saved["serial"],
            saved["mac"]
        );
        ensure!(
            saved.get("id_kind").is_none(),
            "id_kind {} written for a serial number key",
            saved["id_kind"]
        );
        // After the read, the entities follow what the receiver lists.
        let end = Instant::now() + Duration::from_secs(10);
        let tuner = |id: &&String| tuner_differs && id.contains(".tuner_");
        let after = loop {
            let ids = entity_ids(ctx).await?;
            let same = ids.symmetric_difference(&expected).all(|id| tuner(&id));
            if same || Instant::now() > end {
                break ids;
            }
            sleep(Duration::from_millis(200)).await;
        };
        let missing: Vec<_> = expected
            .difference(&after)
            .filter(|id| !tuner(id))
            .collect();
        let other: Vec<_> = after
            .difference(&expected)
            .filter(|id| !tuner(id))
            .collect();
        ensure!(
            missing.is_empty() && other.is_empty(),
            "entities differ after the read with the key {serial}: missing {missing:?}, not expected {other:?}"
        );
        Ok(format!(
            "same {} entities with the key {serial} ({} tuner entities differ), state {state}, mac {} added",
            expected.len(),
            after.symmetric_difference(&expected).count(),
            saved["mac"]
        ))
    }
    .await;
    ctx.key = mac_key;
    let undo = config_back(ctx, good).await;
    first_error(result, undo)
}

/// Stop the driver, write the saved configuration file `good` and start the driver again.
async fn config_back(ctx: &mut Ctx, good: String) -> Result<(), Fail> {
    ctx.driver.stop().await;
    tokio::fs::write(ctx.driver.config_file(), good).await?;
    restart(ctx).await?;
    subscribe_all(ctx).await?;
    settle(ctx).await
}

/// Name and address of the receiver that is added but not used. 192.0.2.1 is a documentation
/// address: nothing answers there.
const UNUSED_NAME: &str = "Unused receiver";
const UNUSED_ADDRESS: &str = "192.0.2.1";

/// A second receiver without subscribed entities is not connected. It connects once an entity
/// is subscribed, and disconnects when the last one is unsubscribed.
pub async fn unused_receiver(ctx: &mut Ctx) -> Check {
    let good = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    let mut config = read_config(ctx).await?;
    let mut unused = config_device(ctx, &config)?.clone();
    unused["id"] = json!("0000000000F1");
    unused["id_kind"] = json!("mac");
    unused["mac"] = json!("0000000000F1");
    unused["serial"] = json!("UNUSED000001");
    unused["name"] = json!(UNUSED_NAME);
    unused["address"] = json!(UNUSED_ADDRESS);
    config
        .as_array_mut()
        .ok_or_else(|| Fail::new("config.json is not a list"))?
        .push(unused);
    ctx.driver.stop().await;
    // The driver is stopped, so the log does not grow.
    let from = ctx.driver.log_size().await;
    let result = async {
        tokio::fs::write(ctx.driver.config_file(), config.to_string()).await?;
        ctx.driver.start().await?;
        ctx.client = crate::ws::Client::connect(ctx.driver.port).await?;
        let ids = entity_ids(ctx).await?;
        let (used, other): (Vec<String>, Vec<String>) =
            ids.into_iter().partition(|id| id.ends_with(ctx.key.as_str()));
        ensure!(!other.is_empty(), "no entities of the unused receiver");
        ctx.client
            .request_ok("subscribe_events", json!({ "entity_ids": used }))
            .await?;
        ctx.client.send_event("connect").await?;
        ctx.client.refresh_states().await?;
        wait_available(ctx, 20).await?;
        ctx.client.listen(3).await?;
        // The name in brackets starts the lines of its actor. The entity list holds the name
        // without brackets.
        let name = format!("[{UNUSED_NAME}]");
        let tried = |log: &str| log.contains(UNUSED_ADDRESS) || log.contains(&name);
        ensure!(
            !tried(&ctx.driver.log_from(from).await),
            "the unused receiver was connected"
        );
        // A subscribed entity makes it used.
        let from = ctx.driver.log_size().await;
        let one = other[0].clone();
        ctx.client
            .request_ok("subscribe_events", json!({ "entity_ids": [one] }))
            .await?;
        ctx.client.listen(2).await?;
        ensure!(
            tried(&ctx.driver.log_from(from).await),
            "no connect after the subscribe"
        );
        let from = ctx.driver.log_size().await;
        ctx.client
            .request_ok("unsubscribe_events", json!({ "entity_ids": [one] }))
            .await?;
        ctx.client.listen(1).await?;
        let log = ctx.driver.log_from(from).await;
        ensure!(
            log.contains(&format!("[{UNUSED_NAME}] not in use, disconnecting")),
            "no disconnect after the unsubscribe"
        );
        Ok("not connected without subscriptions; connected after a subscribe, disconnected after the unsubscribe".to_owned())
    }
    .await;
    let undo = config_back(ctx, good).await;
    first_error(result, undo)
}

pub async fn broken_config(ctx: &mut Ctx) -> Check {
    let good = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    ctx.driver.stop().await;
    let result = async {
        tokio::fs::write(ctx.driver.config_file(), "[{\"broken\": ").await?;
        // Without a receiver the driver needs a setup: it does not give entities.
        ctx.driver.start().await?;
        ctx.client = crate::ws::Client::connect(ctx.driver.port).await?;
        ctx.client
            .request_ok("get_driver_version", Value::Null)
            .await?;
        ensure!(ctx.driver.alive(), "the driver ended");
        let moved = bad_configs(ctx).await?.len();
        ensure!(moved == 1, "{moved} moved files");
        Ok("the driver runs, the file was moved".to_owned())
    }
    .await;
    ctx.driver.stop().await;
    for path in bad_configs(ctx).await.unwrap_or_default() {
        let _ = tokio::fs::remove_file(path).await;
    }
    let undo = config_back(ctx, good).await;
    first_error(result, undo)
}

// ---------------------------------------------------------------- end

/// Set the power and the mute state back to the start. A receiver that was off is switched off.
/// A receiver that was on is switched on if it is off. With the second connection, the volume is
/// lowered to the start volume (not above [`MAX_VOLUME`]) first, then the mute state is set back; an unmute goes through [`unmute_safely`] with the start
/// volume. Does nothing without a device key (set by `entities`), and does not switch on without
/// the start values (read by `states`) or without the second connection (see [`power_on`]).
pub async fn restore_power(ctx: &mut Ctx) -> Check {
    if ctx.key.is_empty() {
        return Ok("no device key, nothing to do".into());
    }
    let mp = ctx.mp();
    if !ctx.was_on {
        ctx.client
            .command_ok("media_player", &mp, "off", json!({}))
            .await?;
        ctx.client.wait_text(&mp, "state", 15, "OFF").await?;
        sleep(Duration::from_millis(200)).await;
        return Ok("the receiver is off again, like at the start".into());
    }
    let Some((muted, volume)) = ctx.start.as_ref().map(|s| (s.muted, s.volume)) else {
        return Ok(String::new());
    };
    let mut notes: Vec<String> = Vec::new();
    ctx.client.listen_ms(300).await?;
    if let Some(note) = volume_answer(ctx).await {
        notes.push(note);
    }
    let mut errors = Ok(());
    if !is_on(&ctx.client.text(&mp, "state")) || receiver_off(ctx) {
        let on = on_again(ctx, volume).await;
        if on.as_ref().is_err_and(|e| e.skip) {
            return on.map(|()| String::new());
        }
        if on.is_ok() {
            notes.push("the receiver is on again, like at the start".into());
        }
        errors = on;
    } else if ctx.oracle.is_some() {
        errors = lower_volume(ctx, volume).await;
    }
    // The driver or the second connection can show another mute state.
    let receiver_muted = ctx
        .oracle
        .as_ref()
        .and_then(|oracle| oracle.state().zone(Zone::Main).muted);
    if muted.is_none() {
        notes.push("the mute state at the start is not known, it is not set back".into());
    }
    if let Some(muted) = muted
        && (ctx.client.attr(&mp, "muted") != Some(&json!(muted))
            || receiver_muted.is_some_and(|m| m != muted))
    {
        let mute = set_muted(ctx, muted, volume, true).await;
        if mute.is_ok() {
            notes.push(
                if muted {
                    "muted again"
                } else {
                    "unmuted again"
                }
                .into(),
            );
        }
        errors = join_errors(errors, mute);
    }
    errors.map(|()| notes.join(", "))
}

/// Time in which [`restore_power`] starts new volume queries. Right after a
/// network restart the receiver can answer late.
const VOLUME_RETRY: Duration = Duration::from_secs(30);

/// Ask the receiver for the volume until it answers. A new query starts only within
/// [`VOLUME_RETRY`], and each query waits for its answer. Gives a note if it answered only after
/// more than one query, or not at all. Does nothing without the second connection.
async fn volume_answer(ctx: &Ctx) -> Option<String> {
    let oracle = ctx.oracle.as_ref()?;
    let start = Instant::now();
    let mut tries = 0u32;
    loop {
        tries += 1;
        match oracle.fresh_volume().await {
            Ok(_) if tries == 1 => return None,
            Ok(_) => {
                return Some(format!(
                    "the receiver answered the volume query after {tries} queries"
                ));
            }
            Err(e) if start.elapsed() >= VOLUME_RETRY => {
                return Some(format!("no volume after {tries} queries: {e}"));
            }
            Err(_) => sleep(Duration::from_secs(1)).await,
        }
    }
}

/// The panic line of the driver log and the line after it, which holds the panic message.
fn panic_line(log: &str) -> Option<String> {
    let mut lines = log.lines();
    let line = lines.find(|l| l.starts_with("thread '") && l.contains(" panicked at "))?;
    Some(match lines.next() {
        Some(message) => format!("{line} {message}"),
        None => line.to_owned(),
    })
}

pub async fn log(ctx: &mut Ctx) -> Check {
    let text = ctx.driver.log_text().await;
    let lines = |tag: &str| -> Vec<&str> { text.lines().filter(|l| l.contains(tag)).collect() };
    // The broken file check causes one ERROR line.
    let errors: Vec<&str> = lines(" ERROR ")
        .into_iter()
        .filter(|l| !l.contains("The file was moved to"))
        .collect();
    let warnings = lines(" WARN ");
    for line in errors.iter().chain(&warnings) {
        println!("      {line}");
    }
    if let Some(line) = panic_line(&text) {
        return Err(Fail::new(format!(
            "the driver panicked: {line}; {} ERROR lines in the log",
            errors.len()
        )));
    }
    ensure!(errors.is_empty(), "{} ERROR lines in the log", errors.len());
    // The unreachable and broken file checks cause some warnings. Without an answer, or after an
    // error of the prompt (for example a closed WebSocket), the warnings do not fail the check.
    let mut unasked = String::new();
    if !warnings.is_empty() {
        let kind = Ask::Question(
            "all are expected",
            "one is not expected",
            "do not check them",
        );
        let question = "Is each WARN line above from a check that was expected to warn?";
        match ask(ctx, kind, question).await {
            Ok(answer) => {
                ensure!(
                    answer != Answer::No,
                    "the owner found an unexpected WARN line in {} warnings",
                    warnings.len()
                );
            }
            Err(e) => unasked = format!(" (the WARN lines are not checked by the owner: {e})"),
        }
    }
    Ok(format!(
        "{} warnings, no errors{unasked}. Log: {}",
        warnings.len(),
        ctx.driver.log.display()
    ))
}

// ---------------------------------------------------------------- sweeps

const SENSORS: [&str; 29] = [
    "volume_db",
    "volume_absolute",
    "sound_mode",
    "input_source",
    "mute",
    "dimmer",
    "eco_mode",
    "sleep_timer",
    "audio_delay",
    "audio_signal",
    "audio_sampling_rate",
    "monitor_output",
    "video_hdmi_signal_in",
    "video_hdmi_signal_out",
    "input_channels",
    "output_channels",
    "max_resolution",
    "hdr_input",
    "hdr_output",
    "pixel_depth_input",
    "pixel_depth_output",
    "max_frl_input",
    "max_frl_output",
    "colorspace_input",
    "colorspace_output",
    "picture_mode",
    "tuner_frequency",
    "all_zone_stereo",
    "digital_codec",
];

/// What happened when one option was selected.
enum Outcome {
    /// The select and its sensor showed the option, and the receiver has the same value.
    Verified,
    /// The driver refused the option with a reason: a rule of the receiver.
    Refused(String),
    /// The command was accepted, but the receiver did not change, and the driver shows what the
    /// receiver has.
    Ignored,
    /// There is no value of the receiver to compare with. False if the select did not show the
    /// option.
    Unverifiable(bool),
}

/// Counts per kind of select.
#[derive(Default)]
struct Tally {
    verified: usize,
    refused: Vec<String>,
    ignored: usize,
    /// The options that the receiver ignored, and the ones without a value to compare.
    detail: Vec<String>,
    unverifiable: usize,
    /// Unverifiable options that the select did not show.
    not_shown: usize,
}

impl Tally {
    fn add(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Verified => self.verified += 1,
            Outcome::Refused(reason) => {
                if !self.refused.contains(&reason) {
                    self.refused.push(reason);
                }
            }
            Outcome::Ignored => self.ignored += 1,
            Outcome::Unverifiable(shown) => {
                self.unverifiable += 1;
                self.not_shown += usize::from(!shown);
            }
        }
    }

    fn describe(&self) -> String {
        let mut parts = vec![format!("{} verified", self.verified)];
        if !self.refused.is_empty() {
            parts.push(format!("refused: {}", self.refused.join("; ")));
        }
        if self.ignored > 0 {
            parts.push(format!(
                "{} ignored by the receiver: {}",
                self.ignored,
                self.detail.join(", ")
            ));
        }
        if self.unverifiable > 0 {
            parts.push(format!(
                "{} unverifiable ({} not shown by the select)",
                self.unverifiable, self.not_shown
            ));
        }
        parts.join(", ")
    }
}

/// The receiver has a value of this select, so the driver can be compared with it.
fn receiver_has(ctx: &Ctx, kind: &str) -> bool {
    let name = format!("select.{kind}");
    ctx.oracle.as_ref().is_some_and(|oracle| {
        oracle::expected(&oracle.state())
            .iter()
            .any(|(n, v)| *n == name && v.is_some())
    })
}

/// Select every option of one select, then the first one again. Adds what went wrong to
/// `problems`. `None` if the select has no options.
async fn sweep_select(
    ctx: &mut Ctx,
    kind: &str,
    problems: &mut Vec<String>,
) -> Result<Option<Tally>, Fail> {
    let select = ctx.select(kind);
    let Some(options) = ctx
        .client
        .attr(&select, "options")
        .and_then(Value::as_array)
        .cloned()
    else {
        problems.push(format!("{kind}: no options"));
        return Ok(None);
    };
    let original = ctx
        .client
        .attr(&select, "current_option")
        .cloned()
        .unwrap_or(Value::Null);
    let mut tally = Tally::default();
    // The library says before sending if it will refuse: the driver must agree.
    let feature = match kind {
        "reference_level_offset" => Some(denon_marantz::Feature::ReferenceLevelOffset),
        "dynamic_volume" => Some(denon_marantz::Feature::Audyssey),
        _ => None,
    };
    let status = match (&ctx.oracle, feature) {
        (Some(oracle), Some(feature)) => {
            oracle.refresh().await?;
            Some(oracle.status(feature))
        }
        _ => None,
    };
    for option in options
        .iter()
        .filter(|o| **o != json!("--") && **o != original)
    {
        match sweep_one(ctx, kind, option).await {
            Ok(outcome) => {
                if matches!(outcome, Outcome::Ignored) {
                    tally.detail.push(format!(
                        "{option} (from {})",
                        ctx.client.text(&select, "current_option")
                    ));
                }
                tally.add(outcome);
            }
            Err(e) => problems.push(format!("{kind} -> {option}: {e}")),
        }
    }
    if original != Value::Null
        && original != json!("--")
        && let Err(e) = sweep_one(ctx, kind, &original).await
    {
        problems.push(format!("{kind} back to {original}: {e}"));
    }
    if let Some(status) = status {
        let refused = !tally.refused.is_empty();
        if refused == status.can_send()
            && tally.verified + tally.ignored + tally.unverifiable + tally.refused.len() > 0
        {
            problems.push(format!(
                "{kind}: the library says {status:?}, the driver {}",
                if refused { "refused" } else { "did not refuse" }
            ));
        }
    }
    Ok(Some(tally))
}

/// Select every option of the selects and check the entities after each change.
pub async fn select_sweep(ctx: &mut Ctx) -> Check {
    // Without the second connection, Dynamic EQ cannot be set back.
    if ctx.oracle.is_none() {
        return Err(Fail::skip(
            "no second connection: Dynamic EQ could not be set back",
        ));
    }
    ensure_on(ctx).await?;
    let mut problems = Vec::new();
    // The receiver ignores the Audyssey settings while a Dirac filter is selected.
    let dirac = ctx.select("dirac_filter");
    let dirac_original = ctx
        .client
        .attr(&dirac, "current_option")
        .cloned()
        .unwrap_or(Value::Null);
    // The saved Dynamic EQ answer lines. `None` until they are read.
    let mut dynamic_eq: Option<Vec<String>> = None;
    let result = async {
        if dirac_original != json!("Off") && dirac_original != Value::Null {
            sweep_one(ctx, "dirac_filter", &json!("Off")).await?;
        }
        // The reference level needs Dynamic EQ. It is set back at the end.
        if let Some(oracle) = &ctx.oracle {
            dynamic_eq = Some(restore::read(oracle.avr(), &["PSDYNEQ ?"]).await?);
        }
        let mut summary = Vec::new();
        // Dynamic EQ is switched on only if its value was saved: else it cannot be set back.
        if dynamic_eq
            .as_ref()
            .is_some_and(|lines| lines.iter().any(|l| l.starts_with("PSDYNEQ")))
        {
            let remote = ctx.remote();
            ctx.client
                .command_ok(
                    "remote",
                    &remote,
                    "send_cmd",
                    json!({ "command": "DYNAMIC_EQ_ON" }),
                )
                .await?;
            ctx.client.listen(1).await?;
        } else {
            summary.push(
                "Dynamic EQ not switched on: the receiver did not answer its query".to_owned(),
            );
        }
        let mut verified = 0;
        // The driver leaves out the selects of features that the receiver does not list.
        let offered = entity_ids(ctx).await?;
        for kind in [
            "dimmer",
            "eco_mode",
            "reference_level_offset",
            "dynamic_volume",
            "monitor_output",
            "speaker_preset",
            "picture_mode",
            "digital_codec",
            "dirac_filter",
        ] {
            if !ctx.opt.kinds.is_empty() && !ctx.opt.kinds.iter().any(|k| k == kind) {
                continue;
            }
            if !offered.contains(&ctx.select(kind)) {
                summary.push(format!("{kind}: not offered"));
                continue;
            }
            if let Some(tally) = sweep_select(ctx, kind, &mut problems).await? {
                verified += tally.verified;
                summary.push(format!("{kind}: {}", tally.describe()));
            }
        }
        Ok((summary, verified))
    }
    .await;
    let sweep = if result.is_ok() && problems.is_empty() {
        Sweep::Clean
    } else {
        Sweep::Failed
    };
    let undo = sweep_back(ctx, &dirac_original, dynamic_eq.as_deref(), sweep).await;
    let (summary, verified) = first_error(result, undo).map_err(|e| {
        if problems.is_empty() {
            e
        } else {
            Fail::new(format!("{e}. Other problems: {}", problems.join(" | ")))
        }
    })?;
    verify_fresh(ctx)
        .await
        .map_err(|e| Fail::new(format!("at the end: {e}")))?;
    if !problems.is_empty() {
        return Err(Fail::new(format!(
            "{} problems: {}",
            problems.len(),
            problems.join(" | ")
        )));
    }
    ensure!(
        verified > 0,
        "nothing was verified. {}",
        summary.join(" | ")
    );
    Ok(format!("{verified} verified. {}", summary.join(" | ")))
}

/// How the select sweep ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sweep {
    /// No error and no problem.
    Clean,
    /// An error or a problem.
    Failed,
}

/// Whether [`sweep_back`] sends the Dirac filter `dirac`. It is not sent if the start filter is
/// not known, or if the sweep is [`Sweep::Clean`] and the driver shows `dirac` (`shown`): the
/// driver and the receiver were then compared, if the receiver gives the filter.
fn dirac_back_needed(dirac: &Value, shown: Option<&Value>, sweep: Sweep) -> bool {
    *dirac != Value::Null
        && dirac.as_str() != Some("--")
        && !(sweep == Sweep::Clean && shown == Some(dirac))
}

/// Set the Dirac filter back to `dirac` if [`dirac_back_needed`], then Dynamic EQ to the saved
/// answer lines. A refused or ignored Dirac filter counts as not set back.
async fn sweep_back(
    ctx: &mut Ctx,
    dirac: &Value,
    dynamic_eq: Option<&[String]>,
    sweep: Sweep,
) -> Result<(), Fail> {
    let mut problems = Vec::new();
    let shown = ctx
        .client
        .attr(&ctx.select("dirac_filter"), "current_option");
    if dirac_back_needed(dirac, shown, sweep) {
        match sweep_one(ctx, "dirac_filter", dirac).await {
            Ok(Outcome::Refused(reason)) => {
                problems.push(format!("dirac_filter back to {dirac}: refused: {reason}"));
            }
            Ok(Outcome::Ignored) => {
                problems.push(format!(
                    "dirac_filter back to {dirac}: ignored by the receiver"
                ));
            }
            Ok(_) => {}
            Err(e) => problems.push(format!("dirac_filter back to {dirac}: {e}")),
        }
    }
    if let (Some(oracle), Some(saved)) = (&ctx.oracle, dynamic_eq) {
        match restore::write(oracle.avr(), &["PSDYNEQ ?"], saved)
            .await
            .map(restore::Missed::into_lines)
        {
            Ok(missed) if missed.is_empty() => {}
            Ok(missed) => problems.push(format!("Dynamic EQ not set back: {missed:?}")),
            Err(e) => problems.push(format!("Dynamic EQ not set back: {e}")),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Fail::new(problems.join(" | ")))
    }
}

/// Set one option, wait for the select and its sensor, then compare with the receiver.
async fn sweep_one(ctx: &mut Ctx, kind: &str, option: &Value) -> Result<Outcome, Fail> {
    let select = ctx.select(kind);
    let log_start = ctx.driver.log_size().await;
    let (response, _) = ctx
        .client
        .command(
            "select",
            &select,
            "select_option",
            json!({ "option": option }),
        )
        .await?;
    if [409, 503, 501].contains(&response["code"].as_u64().unwrap_or(0)) {
        let reason = response["msg_data"]["message"]
            .as_str()
            .unwrap_or("no reason");
        return Ok(Outcome::Refused(reason.to_owned()));
    }
    // A command that the receiver state does not allow is answered with OK, and the driver logs
    // the reason.
    const IGNORED: &str = "select_option ignored: ";
    if response["code"] == 200
        && let Some(reason) = log_after(ctx, log_start)
            .await
            .split_once(IGNORED)
            .and_then(|(_, rest)| rest.lines().next())
    {
        return Ok(Outcome::Refused(reason.to_owned()));
    }
    ensure!(
        response["code"] == 200,
        "code {}, {}",
        response["code"],
        response["msg_data"]
    );
    let wanted = shown(Some(option));
    // A preset change reloads many values: many events follow.
    let wait = if kind == "speaker_preset" { 20 } else { 8 };
    let reached = ctx
        .client
        .wait_attr(&select, "current_option", wait, |v| {
            shown(Some(v)) == wanted
        })
        .await;
    if let Err(e) = reached {
        // The receiver can ignore a setting. The driver is right if it shows what the receiver
        // has. Without a value of the receiver, nothing is known.
        if !receiver_has(ctx, kind) {
            return Ok(Outcome::Unverifiable(false));
        }
        return match verify_all(ctx, 3).await {
            Ok(()) => Ok(Outcome::Ignored),
            Err(_) => Err(e),
        };
    }
    if kind == "speaker_preset"
        && let Some(oracle) = &ctx.oracle
    {
        oracle.ask_after_preset().await?;
    }
    if SENSORS.contains(&kind) {
        let sensor = ctx.sensor(kind);
        ctx.client
            .wait_attr(&sensor, "value", 6, |v| shown(Some(v)) == wanted)
            .await?;
    }
    // Without a value of the receiver for this select, the other entities are still compared:
    // they show the values that depend on it.
    verify_all(ctx, 6).await?;
    if !receiver_has(ctx, kind) {
        return Ok(Outcome::Unverifiable(true));
    }
    Ok(Outcome::Verified)
}

/// Names that the sweep does not send: they recall settings with a volume, store settings or
/// reset them. Triggers can switch other equipment, and the remote lock has no query, so it
/// cannot be set back. The receiver answers the LFE level of the 7.1 input only while that input
/// is used, so it cannot be set back either. A tuner preset goes to a stored frequency: preset up
/// and down do not come back to the start frequency, and the sweep does not set the frequency
/// back. `RESET` counts only as a whole part between `_`, so the speaker and Auro-Matic 3D presets
/// are sent: [`restore::QUERIES`] sets them back.
fn skipped(name: &str) -> bool {
    name.split('_').any(|part| part == "RESET")
        || [
            "ZONE_FAVORITE",
            "QUICK_",
            "SMART_",
            "TUNER_PRESET",
            "MEMORY",
            "VOLUME",
            "NETWORK_RESTART",
            "TRIGGER",
            "REMOTE_CONTROL_LOCK",
            "LFE_7_1_IN",
        ]
        .iter()
        .any(|p| name.contains(p))
}

/// Check the save and restore of `simple_commands`: save the settings, change the sleep timer, set
/// the settings back and check that they all came back. The sleep timer works on every input and
/// does not change the sound.
pub async fn settings_restore(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let Some(oracle) = &ctx.oracle else {
        return Err(Fail::skip("needs the second connection"));
    };
    restore::ensure_nothing_left()?;
    let avr = oracle.avr();
    let mut saved = restore::read(avr, restore::QUERIES).await?;
    restore::ensure_source(&saved)?;
    // The check does not change the source. A marked line is never sent, so a file left by a stop
    // never selects a source.
    restore::mark_source(&mut saved);
    let sleep = saved
        .iter()
        .find(|l| l.starts_with("SLP"))
        .cloned()
        .ok_or_else(|| Fail::skip(format!("no sleep timer value. Read: {saved:?}")))?;
    restore::save(&saved).await?;
    let other = ["SLP120", "SLP090"]
        .into_iter()
        .find(|l| *l != sleep)
        .unwrap_or("SLP120");
    // Set right before the set back, which can send lines and then fail.
    let mut writing = false;
    let result = async {
        avr.telnet_send(Zone::Main, Command::Raw(other)).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let changed = restore::read(avr, &["SLP?"]).await?;
        // The timer counts down: a value near the set one is also a change.
        ensure!(
            changed.iter().any(|l| l != &sleep && l != "SLPOFF"),
            "the sleep timer did not change to {other}: {changed:?}"
        );
        writing = true;
        restore::write(avr, restore::QUERIES, &saved).await
    }
    .await;
    let mut missed = match result {
        Ok(missed) => missed,
        Err(e) => {
            // A failed send can still reach the receiver, the receiver can take the new timer
            // after the read, or the set back can stop early.
            let undo = sleep_back(avr, &sleep).await;
            // The check changes only the sleep timer. A set back that stopped while it sent can
            // leave other settings changed, for example Dirac: the file stays.
            if undo.is_ok() && !writing {
                restore::forget().await;
            }
            let e = if writing {
                Fail::new(format!(
                    "{e}. The settings stay in {}",
                    restore::FILE.display()
                ))
            } else {
                e
            };
            return first_error(Err(e), undo);
        }
    };
    // The write stopped because of the source. The sleep timer is the same for every source, so
    // it is set back. The check changes no other setting, so then nothing is left to set back.
    if matches!(missed, restore::Missed::NotSent(_)) {
        return Err(match sleep_back(avr, &sleep).await {
            Ok(()) => {
                restore::forget().await;
                Fail::new(
                    "the set back stopped because of the source. The sleep timer was set back",
                )
            }
            Err(e) => Fail::new(format!(
                "the set back stopped because of the source. Not set back: {e}. The settings stay in {}",
                restore::FILE.display()
            )),
        });
    }
    let sound = missed.take_sound_mode();
    let missed = missed.into_lines();
    let body = if missed.is_empty() {
        Ok(())
    } else {
        Err(Fail::new(format!(
            "not set back: {missed:?}{}. They stay in {}",
            if sound.is_empty() {
                String::new()
            } else {
                format!(", {sound:?}")
            },
            restore::FILE.display()
        )))
    };
    body?;
    restore::forget().await;
    ensure!(
        !restore::FILE.exists(),
        "{} was not removed",
        restore::FILE.display()
    );
    let sound_note = if sound.is_empty() {
        String::new()
    } else {
        format!(", sound mode not taken: {sound:?}{}", restore::SOUND_NOTE)
    };
    Ok(format!(
        "{} settings read, {sleep} -> {other} -> {sleep}, all set back{sound_note}",
        saved.len()
    ))
}

/// Set the sleep timer back to the saved line `sleep`.
async fn sleep_back(avr: &denon_marantz::Avr, sleep: &str) -> Result<(), Fail> {
    let missed = restore::write(avr, &["SLP?"], &[sleep.to_owned()])
        .await?
        .into_lines();
    ensure!(missed.is_empty(), "sleep timer: {missed:?}");
    Ok(())
}

/// Send every simple command of the remote entity, and check the entities against the receiver.
/// The settings that the commands change and the source are saved first and set back at the end,
/// also when the check fails. The source is set back only if it may be selected.
pub async fn simple_commands(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let Some(oracle) = &ctx.oracle else {
        return Err(Fail::skip(
            "needs the second connection: it saves the settings",
        ));
    };
    restore::ensure_nothing_left()?;
    let mut saved = restore::read(oracle.avr(), restore::QUERIES).await?;
    restore::ensure_source(&saved)?;
    let start = restore::saved_source(&saved);
    let plan = match read_sources(ctx).await {
        Ok(sources) => SourcePlan::new(&source_rule(ctx, &sources), start),
        Err(why) => SourcePlan::none(why),
    };
    // The file never holds a source that may not be selected: the next run would select it.
    if !plan.start_ok {
        restore::mark_source(&mut saved);
    }
    restore::save(&saved).await?;
    let result = send_simple_commands(ctx, &plan).await;
    let oracle = ctx
        .oracle
        .as_ref()
        .ok_or_else(|| Fail::new("the second connection is gone"))?;
    // The file stays if a setting did not come back: the next run sends them again. A sound mode
    // that the receiver did not take does not keep the file.
    let mut missed = restore::write(oracle.avr(), restore::QUERIES, &saved).await?;
    let sound = missed.take_sound_mode();
    let sent = matches!(missed, restore::Missed::Lines(_));
    let mut missed = missed.into_lines();
    if missed.is_empty() {
        restore::forget().await;
    }
    // The receiver often does not take the audio restorer back here, but takes it at the start of
    // the next run. It is not a failure: the line stays in the file for the next run.
    let late: Vec<String> = if sent {
        missed
            .extract_if(.., |line| line.starts_with("PSRSTR"))
            .collect()
    } else {
        Vec::new()
    };
    let mut late_note = if late.is_empty() {
        String::new()
    } else {
        format!(". Not taken: {late:?}. The next run sets them back")
    };
    if !sound.is_empty() {
        let _ = write!(
            late_note,
            ". Sound mode not taken: {sound:?}{}",
            restore::SOUND_NOTE
        );
    }
    match (result, missed.is_empty()) {
        (Ok(note), true) => Ok(format!(
            "{note}, {} settings set back{late_note}",
            saved.len() - late.len() - sound.len()
        )),
        (Ok(note), false) => Err(Fail::new(format!(
            "{note}. Settings not set back: {missed:?}{late_note}"
        ))),
        (Err(mut e), true) => {
            e.msg.push_str(&late_note);
            Err(e)
        }
        (Err(e), false) => Err(Fail::new(format!(
            "{}. Settings not set back: {missed:?}{late_note}",
            e.msg
        ))),
    }
}

/// Name, brand and text of a line of the command table.
fn command_fields(line: &str) -> Option<(&str, &str, &str)> {
    let mut parts = line.split('\t');
    let name = parts.next()?;
    let brand = parts.nth(1)?;
    Some((name, brand, parts.next().unwrap_or_default()))
}

async fn send_simple_commands(ctx: &mut Ctx, plan: &SourcePlan) -> Check {
    let remote = ctx.remote();
    let list = entity_list(ctx).await?;
    let offered = strings(
        &list
            .iter()
            .find(|e| e["entity_id"] == remote.as_str())
            .map_or(Value::Null, |e| e["options"]["simple_commands"].clone()),
    );
    let table = tokio::fs::read_to_string(&ctx.opt.commands)
        .await
        .map_err(|e| Fail::new(format!("{}: {e}", ctx.opt.commands.display())))?;
    // The sources of the source (`SI`) commands. The table built into the driver counts too, in
    // case `--commands` is another file.
    let source_codes: Vec<(&str, &str)> = COMMANDS
        .lines()
        .chain(table.lines())
        .filter_map(command_fields)
        .filter_map(|(name, _, text)| Some((name, text.strip_prefix("SI")?)))
        .collect();
    let mut source_count = 0;
    let mut names: Vec<&str> = table
        .lines()
        .filter_map(command_fields)
        .filter(|(_, brand, _)| *brand != "marantz")
        .map(|(name, _, _)| name)
        .filter(|n| offered.iter().any(|o| o == n) && !skipped(n))
        .filter(|n| {
            let codes: Vec<&str> = source_codes
                .iter()
                .filter(|(name, _)| name == n)
                .map(|(_, code)| *code)
                .collect();
            let ok = plan.command_ok(n, &codes);
            source_count += usize::from(ok && !codes.is_empty());
            ok
        })
        .collect();
    // Source and tuner commands go last: some settings are kept for each source, so the other
    // commands change the settings of the start source, which the set back compares.
    names.sort_by_key(|n| changes_source(n, &source_codes));
    let tuner_offered = offered.iter().any(|o| o.starts_with("TUNER_"));
    let source_note = if plan.start_ok && (plan.tuner_ok || !tuner_offered) {
        format!(", {source_count} source commands")
    } else if plan.start_ok {
        format!(", {source_count} source commands, no tuner command")
    } else {
        format!(", no source or tuner command: {}", plan.why)
    };
    ensure!(names.len() > 250, "only {} commands to send", names.len());
    let mut problems = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    // No command may leave the volume above the volume of the receiver at the start, or above
    // the limit. A start volume above the limit is lowered once.
    let start = receiver_volume(ctx)
        .await?
        .ok_or_else(|| Fail::new("no second connection"))?;
    let mut start_note = String::new();
    if start > MAX_VOLUME {
        lower_volume(ctx, None).await.map_err(|e| {
            Fail::new(format!(
                "the start volume {start} is above the limit {MAX_VOLUME}, not lowered: {e}"
            ))
        })?;
        start_note = format!(". The start volume {start} was lowered to {MAX_VOLUME}");
    }
    let baseline = Some(start.min(MAX_VOLUME));
    let limit = unmute_limit(baseline);
    // A speaker preset command was sent since the last compare.
    let mut preset_sent = false;
    let mut sent = names.len();
    for (i, name) in names.iter().enumerate() {
        // A source or tuner command can switch the zone on, at a volume that is lowered only
        // afterwards. These commands are sent last, so the stop skips only them.
        if changes_source(name, &source_codes)
            && ctx
                .oracle
                .as_ref()
                .is_none_or(|o| o.state().zone(Zone::Main).power != Some(Power::On))
        {
            // First, so that the list stays short and still shows it.
            problems.insert(
                0,
                format!(
                    "{name} and the source and tuner commands after it not sent: the main zone is not on"
                ),
            );
            sent = i;
            break;
        }
        let (response, took) = ctx
            .client
            .command("remote", &remote, "send_cmd", json!({ "command": name }))
            .await?;
        if [409, 503, 501].contains(&response["code"].as_u64().unwrap_or(0)) {
            let reason = response["msg_data"]["message"]
                .as_str()
                .unwrap_or("no reason");
            refused.push(format!("{name}: {reason}"));
        } else if response["code"] != 200 {
            problems.push(format!("{name}: code {}", response["code"]));
        } else if took > Duration::from_secs(10) {
            problems.push(format!("{name}: took {} ms", took.as_millis()));
        }
        ctx.client.listen_ms(300).await?;
        preset_sent |= name.starts_with("SPEAKER_PRESET");
        if response["code"] == 200 && !wait_for_filter(ctx, name).await? {
            problems.push(format!("{name}: the receiver did not select the filter"));
        }
        // The cached values decide if the receiver is asked: the second connection follows the
        // changes of the receiver. The receiver is asked before anything is sent.
        let seen = highest(
            current_volume(ctx),
            ctx.oracle.as_ref().and_then(Oracle::volume),
        );
        if seen.is_some_and(|v| above_baseline(v, baseline)) {
            let real = receiver_volume(ctx).await?;
            if real.is_none_or(|v| above_baseline(v, baseline)) {
                let real = real.map_or_else(|| "an unknown volume".to_owned(), |v| v.to_string());
                lower_volume(ctx, baseline).await.map_err(|e| {
                    Fail::new(format!(
                        "{name}: the volume went to {real}, above {limit}, not lowered: {e}. Earlier problems: {}",
                        problems.join(" | ")
                    ))
                })?;
                problems.push(format!(
                    "{name}: the volume went to {real}, above {limit}, it was lowered"
                ));
            }
        }
        if i % 10 == 9 {
            compare_group(ctx, name, preset_sent, &mut problems).await?;
            preset_sent = false;
        }
    }
    verify_fresh(ctx)
        .await
        .map_err(|e| Fail::new(format!("at the end: {e}")))?;
    if problems.is_empty() {
        // The effect of one command is not checked: only that the entities showed the values of
        // the receiver after each group of 10 commands.
        Ok(format!(
            "{} commands accepted ({} refused with a reason: {refused:?}){source_note}, the effect of each is not verified, entities matched the receiver after every 10 commands{start_note}",
            names.len() - refused.len(),
            refused.len()
        ))
    } else {
        problems.truncate(12);
        Err(Fail::new(format!(
            "{sent} commands sent{source_note}. Problems: {}{start_note}",
            problems.join(" | ")
        )))
    }
}

/// A source may be selected: the receiver gives its name, the source names page gives its default
/// name, the receiver reports it as in use, and neither its code nor any of its names contains a
/// name of `--avoid-sources`, without case. Tuner sources only with a tuner. Without
/// `--avoid-sources`, no source.
///
/// A source without a default name is not on the names page, so the name of the device on its
/// input is not known either. Without a name, the receiver may not have sent it yet.
fn may_send_source(opt: &crate::Options, info: &SourceInfo, tuner: bool) -> bool {
    if !tuner && TUNER_CODES.contains(&info.code.as_str()) {
        return false;
    }
    if info.in_use != Some(true) || info.name.is_none() || info.default_name.is_none() {
        return false;
    }
    [
        Some(info.code.as_str()),
        info.name.as_deref(),
        info.default_name.as_deref(),
        info.auto_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(|name| opt.may_select(name))
}

/// The simple command `name` can change the source: a source command or a tuner command.
fn changes_source(name: &str, source_codes: &[(&str, &str)]) -> bool {
    name.starts_with("TUNER_") || source_codes.iter().any(|(n, _)| *n == name)
}

/// Telnet codes of the tuner sources.
const TUNER_CODES: [&str; 2] = ["TUNER", "HDRADIO"];

/// Which source and tuner commands the simple command check sends.
struct SourcePlan {
    /// The start source may be selected. The set back at the end selects it again, so no source
    /// or tuner command is sent without it.
    start_ok: bool,
    /// The start source may be selected, a tuner source exists and each tuner source may be
    /// selected: a tuner command can select the tuner.
    tuner_ok: bool,
    /// Codes of the sources that may be selected.
    allowed: Vec<String>,
    /// Why no source command is sent, when `start_ok` is false.
    why: String,
}

impl SourcePlan {
    /// The plan for the start source with telnet code `start`.
    fn new(rule: &SourceRule<'_>, start: Option<&str>) -> Self {
        let start_ok = start.is_some_and(|code| rule.code_ok(code));
        let mut tuners = TUNER_CODES
            .into_iter()
            .filter(|code| rule.info(code).is_some())
            .peekable();
        let tuner_ok = start_ok && tuners.peek().is_some() && tuners.all(|c| rule.code_ok(c));
        let allowed = rule
            .sources
            .iter()
            .filter(|s| may_send_source(rule.opt, s, rule.tuner))
            .map(|s| s.code.clone())
            .collect();
        let why = if start_ok {
            String::new()
        } else {
            format!(
                "the start source {} may not be selected",
                start.unwrap_or("(unknown)")
            )
        };
        Self {
            start_ok,
            tuner_ok,
            allowed,
            why,
        }
    }

    /// No source or tuner command, for this reason.
    fn none(why: String) -> Self {
        Self {
            start_ok: false,
            tuner_ok: false,
            allowed: Vec::new(),
            why,
        }
    }

    /// The simple command `name` may be sent. `codes` are the sources that it selects: empty if it
    /// is not a source command.
    fn command_ok(&self, name: &str, codes: &[&str]) -> bool {
        if !codes.is_empty() {
            return self.start_ok && codes.iter().all(|c| self.allowed.iter().any(|a| a == c));
        }
        self.tuner_ok || !name.starts_with("TUNER_")
    }
}

/// After a Dirac filter command, wait until the receiver has the filter: the driver refuses tone
/// control commands until it sees the new filter. False if the receiver did not select it.
async fn wait_for_filter(ctx: &mut Ctx, name: &str) -> Result<bool, Fail> {
    let Some(filter) = dirac_filter(name) else {
        return Ok(true);
    };
    if oracle_dirac(ctx).is_none() || empty_slot(ctx, filter) {
        return Ok(true);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while oracle_dirac(ctx) != Some(filter) && Instant::now() < deadline {
        ctx.client.listen_ms(200).await?;
    }
    let selected = oracle_dirac(ctx) == Some(filter);
    ctx.client.listen_ms(300).await?;
    Ok(selected)
}

/// Compare the entities with the receiver after a group of commands. Fails if the receiver is
/// not on anymore.
async fn compare_group(
    ctx: &mut Ctx,
    name: &str,
    preset_sent: bool,
    problems: &mut Vec<String>,
) -> Result<(), Fail> {
    if preset_sent
        && let Some(oracle) = &ctx.oracle
        && let Err(e) = oracle.ask_after_preset().await
    {
        problems.push(format!(
            "after {name}: values of the speaker preset not asked: {e}"
        ));
    }
    if let Err(e) = verify_all(ctx, 6).await {
        problems.push(format!("after {name}: {e}"));
    }
    ensure_on(ctx).await.map_err(|e| {
        Fail::new(format!(
            "after {name}: {e}. Earlier problems: {}",
            problems.join(" | ")
        ))
    })
}

// ---------------------------------------------------------------- extra entities

/// Switch the tone control and change the bass level, then restore both. The triggers and the
/// Bluetooth transmitter are not changed: they can switch other equipment.
pub async fn extra_entities(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    verify_fresh(ctx).await?;
    let mut notes = Vec::new();

    let tone = format!("switch.tone_control.{}", ctx.key);
    let before = ctx.client.text(&tone, "state");
    ensure!(
        before == "ON" || before == "OFF",
        "tone control switch shows {before}"
    );
    let bass = format!("number.bass.{}", ctx.key);
    // What the body can have changed: only these are set back.
    let mut tone_changed = false;
    // The bass level before its change. `None` while the bass is not changed.
    let mut bass_start = None;
    let result = async {
        tone_changed = true;
        let (response, _) = ctx
            .client
            .command("switch", &tone, "toggle", json!({}))
            .await?;
        if [409, 503].contains(&response["code"].as_u64().unwrap_or(0)) {
            tone_changed = false;
            // A Dirac filter blocks the tone control.
            notes.push(format!(
                "tone control refused: {}",
                response["msg_data"]["message"]
                    .as_str()
                    .unwrap_or("no reason")
            ));
        } else {
            ensure!(response["code"] == 200, "toggle: code {}", response["code"]);
            let after = if before == "ON" { "OFF" } else { "ON" };
            if ctx
                .client
                .wait_text(&tone, "state", 6, after)
                .await
                .is_err()
            {
                // The receiver can ignore it, for example in a direct sound mode. The driver is
                // right if it shows what the receiver has.
                verify_all(ctx, 3).await?;
                notes.push("tone control: the receiver did not change it".to_owned());
            } else {
                verify_all(ctx, 6).await?;
                ctx.client
                    .command_ok("switch", &tone, "toggle", json!({}))
                    .await?;
                ctx.client.wait_text(&tone, "state", 6, &before).await?;
                verify_all(ctx, 6).await?;
                notes.push("tone control toggled and restored".to_owned());
            }
        }

        let start = ctx.client.attr(&bass, "value").and_then(Value::as_i64);
        bass_start = start;
        match start {
            None => notes.push("bass has no value".to_owned()),
            Some(start) => {
                let (up, back) = if start < 6 {
                    ("increment", "decrement")
                } else {
                    ("decrement", "increment")
                };
                let moved = if start < 6 { start + 1 } else { start - 1 };
                let (response, _) = ctx.client.command("number", &bass, up, json!({})).await?;
                if response["code"] == 409 {
                    bass_start = None;
                    // A Dirac filter blocks the tone control.
                    notes.push(format!(
                        "bass refused: {}",
                        response["msg_data"]["message"]
                            .as_str()
                            .unwrap_or("no reason")
                    ));
                    return Ok(notes.join(", "));
                }
                ensure!(response["code"] == 200, "{up}: code {}", response["code"]);
                let moved_to = ctx
                    .client
                    .wait_attr(&bass, "value", 6, |v| v.as_i64() == Some(moved))
                    .await;
                if moved_to.is_err() {
                    verify_all(ctx, 3).await?;
                    notes.push("bass: the receiver did not change it".to_owned());
                } else {
                    verify_all(ctx, 6).await?;
                    ctx.client
                        .command_ok("number", &bass, back, json!({}))
                        .await?;
                    ctx.client
                        .wait_attr(&bass, "value", 6, |v| v.as_i64() == Some(start))
                        .await?;
                    verify_all(ctx, 6).await?;
                    notes.push(format!("bass {start} -> {moved} -> {start} dB"));
                }
            }
        }
        Ok(notes.join(", "))
    }
    .await;
    let tone_before = tone_changed.then_some(before.as_str());
    let undo = tone_back(ctx, &tone, tone_before, &bass, bass_start, result.is_err()).await;
    first_error(result, undo)
}

/// Set the tone control switch back to `tone_before`, then the bass level back to `bass_start`.
/// `None` means that the check did not change the value: nothing is sent for it. Without `force`,
/// a value that the driver shows as the old one is not sent.
async fn tone_back(
    ctx: &mut Ctx,
    tone: &str,
    tone_before: Option<&str>,
    bass: &str,
    bass_start: Option<i64>,
    force: bool,
) -> Result<(), Fail> {
    ctx.client.listen_ms(300).await?;
    let tone_result = async {
        if let Some(tone_before) = tone_before
            && (force || ctx.client.text(tone, "state") != tone_before)
        {
            let cmd = if tone_before == "ON" { "on" } else { "off" };
            ctx.client
                .command_ok("switch", tone, cmd, json!({}))
                .await?;
            ctx.client.wait_text(tone, "state", 6, tone_before).await?;
        }
        Ok(())
    }
    .await;
    let bass_result = async {
        if let Some(start) = bass_start
            && (force || ctx.client.attr(bass, "value").and_then(Value::as_i64) != Some(start))
        {
            ctx.client
                .command_ok("number", bass, "set_value", json!({ "value": start }))
                .await?;
            ctx.client
                .wait_attr(bass, "value", 6, |v| v.as_i64() == Some(start))
                .await?;
        }
        Ok(())
    }
    .await;
    join_errors(tone_result, bass_result)
}

/// Time that the HEOS check watches the events of the media player while HEOS plays.
const HEOS_QUIET_SECS: u64 = 12;

/// The position may differ this much from the one of the second HEOS connection, in seconds.
const HEOS_POSITION_SLACK: i64 = 4;

/// HEOS: what plays, the position, the number of events and play and pause, compared with a second
/// HEOS connection. If HEOS does not play on the main zone, the owner is asked to start it, see
/// [`start_heos`]. The check sends no volume command, except the lowering after a power on and
/// the lowering with [`cap_volume`] in [`start_heos`], and in the set back of the source. If the
/// owner was asked to start HEOS, the end steps of [`heos_end`] run, also after a failure: HEOS is
/// paused or stopped and the start source is set back.
pub async fn heos(ctx: &mut Ctx) -> Check {
    use denon_marantz_heos::Heos;
    let mp = ctx.mp();
    let entities = entity_list(ctx).await?;
    let features = entities
        .iter()
        .find(|e| e["entity_id"] == json!(mp))
        .map_or_default(|e| strings(&e["features"]));
    let heos = match Heos::connect(ctx.opt.address.as_str()).await {
        Ok(heos) => heos,
        Err(e) => {
            // While HEOS is not known, the driver offers the HEOS features already.
            let config = read_config(ctx).await?;
            ensure!(
                config_device(ctx, &config)?["heos"] != json!(true),
                "the driver saved that the receiver has HEOS, but HEOS does not answer: {e}"
            );
            return Err(Fail::skip(format!("no HEOS: {e}")));
        }
    };
    let mut started = None;
    let result = heos_compare(ctx, &heos, &features, &mut started).await;
    let result = match started {
        Some(start) => heos_end(ctx, &heos, &start, result).await,
        None => result,
    };
    heos.close().await;
    result
}

/// What [`start_heos`] read before it asked the owner to start HEOS, and if the owner said that
/// HEOS plays.
struct HeosStart {
    /// The HEOS player of the receiver.
    pid: denon_marantz_heos::PlayerId,
    /// The source that the driver showed.
    source: String,
    /// The source list of the driver.
    list: Vec<String>,
    /// The telnet code of the source from the second connection.
    code: Option<String>,
    /// The higher of the receiver and the driver volume, the limit for the set back of the source.
    volume: f64,
    /// The owner said that HEOS plays.
    answered: bool,
}

/// The end steps of [`heos`] after the owner was asked to start HEOS, added to the result
/// `result`: pause or stop HEOS with [`end_heos_play`], then set the start source back with
/// [`heos_source_back`]. The check first reads the events for 300 ms, or for 2 s if the owner
/// did not say that HEOS plays: HEOS can start a moment after the answer.
async fn heos_end(
    ctx: &mut Ctx,
    heos: &denon_marantz_heos::Heos,
    start: &HeosStart,
    result: Check,
) -> Check {
    let settle = ctx
        .client
        .listen_ms(if start.answered { 300 } else { 2000 })
        .await;
    let result = add_back_note(result, end_heos_play(heos, start.pid).await);
    // The source is set back also after a failed read: the owner can still be asked.
    let back = heos_source_back(ctx, start).await;
    add_set_back(first_error(result, settle), back)
}

/// Set the start source of `start` back with [`source_back`] if the driver or the second
/// connection shows another source.
async fn heos_source_back(ctx: &mut Ctx, start: &HeosStart) -> Result<SetBack, Fail> {
    let label = ctx.client.text(&ctx.mp(), "source");
    let code = main_source(ctx);
    if !source_changed(
        &label,
        &start.source,
        code.as_deref(),
        start.code.as_deref(),
    ) {
        return Ok(SetBack::note("the source did not change".to_owned()));
    }
    source_back(ctx, &start.source, &start.list, start.volume).await
}

/// The telnet code of the main zone source from the second connection. `None` without that
/// connection or if it is not known.
fn main_source(ctx: &Ctx) -> Option<String> {
    ctx.oracle
        .as_ref()
        .and_then(|oracle| oracle.state().zone(Zone::Main).source.clone())
}

/// The driver shows the source `label` and not the start source `old`, or the second connection
/// has the code `code` and not the start code `old_code`. Codes are compared only when both are
/// known.
fn source_changed(label: &str, old: &str, code: Option<&str>, old_code: Option<&str>) -> bool {
    label != old || matches!((code, old_code), (Some(c), Some(o)) if c != o)
}

/// The part of [`heos`] after the connect to HEOS: the HEOS connection `heos` and the features
/// `features` of the media player. `started` gets the start values if the owner is asked to start
/// HEOS.
async fn heos_compare(
    ctx: &mut Ctx,
    heos: &denon_marantz_heos::Heos,
    features: &[String],
    started: &mut Option<HeosStart>,
) -> Check {
    use denon_marantz_heos::{PlayState, RepeatMode};
    let mp = ctx.mp();
    ensure!(
        features.iter().any(|f| f == "media_position"),
        "HEOS answers, but the media player has no position feature"
    );
    let ip = ctx
        .opt
        .address
        .parse()
        .map_err(|_| Fail::skip("the address is not an IP address"))?;
    let pid = heos
        .player_by_ip(ip)
        .ok_or_else(|| Fail::new("HEOS has no player with the address of the receiver"))?;
    let player = heos
        .player(pid)
        .ok_or_else(|| Fail::new("HEOS player not read"))?;
    let (player, media) = match heos_playing(&player) {
        Some(media) => (player, media),
        None => start_heos(ctx, heos, pid, started).await?,
    };
    let blank = |t: &str| t.trim().is_empty();
    let station = media.station.as_deref().filter(|t| !blank(t)).unwrap_or("");
    let (title, album) = if blank(&media.song) {
        (station, media.album.as_ref())
    } else if blank(&media.album) {
        (media.song.as_ref(), station)
    } else {
        (media.song.as_ref(), media.album.as_ref())
    };
    ctx.client.wait_text(&mp, "media_title", 8, title).await?;
    ctx.client.wait_text(&mp, "state", 4, "PLAYING").await?;
    ensure!(
        ctx.client.text(&mp, "media_artist") == media.artist.as_ref(),
        "artist {:?}, HEOS has {:?}",
        ctx.client.text(&mp, "media_artist"),
        media.artist
    );
    ensure!(
        ctx.client.text(&mp, "media_album") == album,
        "album {:?}, HEOS has {album:?}",
        ctx.client.text(&mp, "media_album")
    );
    if let Some(mode) = player.play_mode() {
        let repeat = match mode.repeat {
            RepeatMode::OnAll => "ALL",
            RepeatMode::OnOne => "ONE",
            _ => "OFF",
        };
        ensure!(
            ctx.client.text(&mp, "repeat") == repeat,
            "repeat {:?}, HEOS has {repeat}",
            ctx.client.text(&mp, "repeat")
        );
        let shuffle = ctx.client.attr(&mp, "shuffle").and_then(Value::as_bool);
        ensure!(
            shuffle == Some(mode.shuffle),
            "shuffle {shuffle:?}, HEOS has {}",
            mode.shuffle
        );
    }
    ensure!(
        ctx.client.text(&mp, "media_image_url") == media.image_url.as_ref(),
        "image {:?}, HEOS has {:?}",
        ctx.client.text(&mp, "media_image_url"),
        media.image_url
    );

    // The position: the Remote counts on from `media_position_updated_at`.
    let position = heos_position(ctx, &mp)?;
    let fresh = heos
        .player(pid)
        .and_then(|p| Some((p.progress?, heos_now(&p)?)));
    let note_position = if let Some((progress, expected)) = fresh {
        ensure!(
            (position - expected).abs() <= HEOS_POSITION_SLACK,
            "position {position} s, HEOS has {expected} s"
        );
        let duration = ctx
            .client
            .attr(&mp, "media_duration")
            .and_then(Value::as_u64);
        ensure!(
            duration == Some(progress.duration_ms / 1000),
            "duration {duration:?}, HEOS has {} s",
            progress.duration_ms / 1000
        );
        format!("position {position} s")
    } else {
        "HEOS sent no position yet".to_owned()
    };

    // HEOS sends the position every few seconds. The driver sends it only when it jumps. Other
    // events, for example a volume change, are not counted.
    let before = ctx.client.position_count(&mp);
    ctx.client.listen(HEOS_QUIET_SECS).await?;
    let events = ctx.client.position_count(&mp) - before;
    ensure!(
        events <= 1,
        "{events} position events in {HEOS_QUIET_SECS} s while HEOS plays"
    );

    // Play and pause go to HEOS. An item that cannot pause is not paused here: it would stop.
    let note_pause = if player.controls().pause {
        ctx.client
            .command_ok("media_player", &mp, "play_pause", json!({}))
            .await?;
        let played_at = ctx.client.text(&mp, "media_position_updated_at");
        ctx.client.wait_text(&mp, "state", 6, "PAUSED").await?;
        let heos_state = heos.player(pid).and_then(|p| p.play_state);
        ensure!(
            heos_state == Some(PlayState::Pause),
            "the driver shows PAUSED, HEOS has {heos_state:?}"
        );
        // The Remote stops its count at the time of the pause.
        let paused_at = ctx.client.text(&mp, "media_position_updated_at");
        ensure!(
            paused_at != played_at,
            "media_position_updated_at did not change with the pause"
        );
        ctx.client
            .command_ok("media_player", &mp, "play_pause", json!({}))
            .await?;
        ctx.client.wait_text(&mp, "state", 6, "PLAYING").await?;
        ensure!(
            ctx.client.text(&mp, "media_position_updated_at") != paused_at,
            "media_position_updated_at did not change with the play"
        );
        let position = heos_position(ctx, &mp)?;
        if let Some(expected) = heos.player(pid).as_ref().and_then(heos_now) {
            ensure!(
                (position - expected).abs() <= HEOS_POSITION_SLACK,
                "after the play: position {position} s, HEOS has {expected} s"
            );
        }
        "paused and played, the position followed".to_owned()
    } else {
        "the item cannot pause: not paused".to_owned()
    };
    Ok(format!(
        "{title:?}, {note_position}, {events} position events in {HEOS_QUIET_SECS} s, {note_pause}"
    ))
}

/// What the HEOS player plays, if its state is play.
fn heos_playing(
    player: &denon_marantz_heos::PlayerState,
) -> Option<denon_marantz_heos::NowPlaying<'static>> {
    player
        .current_media()
        .filter(|_| player.play_state == Some(denon_marantz_heos::PlayState::Play))
        .cloned()
}

/// Time for the HEOS player to play after the owner started it, in seconds.
const HEOS_START_SECS: u64 = 10;

/// HEOS does not play: switch the receiver on with [`ensure_on`], ask the owner to start HEOS
/// Music, then read the player again and wait for its play state. `started` gets `pid` and the
/// start source before the question. Skips if there is no terminal, without `--avoid-sources` or
/// without the second connection, if [`ensure_on`] skips, if the volume is above [`MAX_VOLUME`] or
/// not known, if [`read_sources`] fails, if [`start_source_ok`] says that the start source cannot
/// be set back, or if the owner does not answer. The source rule does not apply to
/// [`HEOS_SOURCE`]: the owner selects it. Fails if [`ensure_on`] fails, if the volume or the
/// prompt cannot be read, if HEOS does not play [`HEOS_START_SECS`] after the answer, or if the
/// volume is above [`MAX_VOLUME`] once HEOS plays (it is lowered with [`cap_volume`]).
async fn start_heos(
    ctx: &mut Ctx,
    heos: &denon_marantz_heos::Heos,
    pid: denon_marantz_heos::PlayerId,
    started: &mut Option<HeosStart>,
) -> Result<
    (
        denon_marantz_heos::PlayerState,
        denon_marantz_heos::NowPlaying<'static>,
    ),
    Fail,
> {
    const NOT_PLAYING: &str = "HEOS does not play";
    let kind = Ask::Action("it plays", SKIP_CHECK);
    let text = "Start HEOS Music on the receiver. This makes sound.";
    // Without a terminal the receiver is not switched on only to skip.
    if !terminal() {
        ctx.answers
            .push(format!("{text:?}: not asked, no terminal"));
        return Err(Fail::skip(format!("{NOT_PLAYING} (no terminal)")));
    }
    let not_playing = |why: String| Fail::skip(format!("{NOT_PLAYING} ({why})"));
    // Without `--avoid-sources` or the second connection the receiver is not switched on only to
    // skip.
    source_oracle(ctx).map_err(not_playing)?;
    // The receiver is switched on first with the volume check: a power on by HEOS can set a loud
    // power-on volume.
    ensure_on(ctx)
        .await
        .map_err(|e| if e.skip { not_playing(e.msg) } else { e })?;
    let receiver = receiver_volume(ctx).await?;
    let volume = sound_volume(highest(receiver, driver_volume(ctx).await?)).map_err(not_playing)?;
    // The input levels must pass `levels_ok`: a higher level makes the source louder.
    let sources = read_sources(ctx).await.map_err(not_playing)?;
    let rule = source_rule(ctx, &sources);
    // The start source is set back at the end, so it must be one that may be selected.
    let mp = ctx.mp();
    let source = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    start_source_ok(&rule, &source, &list).map_err(not_playing)?;
    let start = started.insert(HeosStart {
        pid,
        source,
        list,
        code: main_source(ctx),
        volume,
        answered: false,
    });
    let answer = ask(ctx, kind, text).await?;
    if answer != Answer::Done {
        return Err(Fail::skip(format!("{NOT_PLAYING} (answer {answer:?})")));
    }
    start.answered = true;
    // A failed read is not a failure: the events of the player can still come.
    let deadline = Instant::now() + Duration::from_secs(HEOS_START_SECS);
    let refresh = heos.refresh().await.err();
    loop {
        // The state is copied only when HEOS plays.
        let playing = heos.with_player(pid, |p| heos_playing(p).map(|m| (p.clone(), m)));
        if let Some(found) = playing.flatten() {
            // The owner can change the volume in the HEOS app.
            if let Some(loud) = cap_volume(ctx).await? {
                return Err(Fail::new(format!("after HEOS started {loud}")));
            }
            return Ok(found);
        }
        if Instant::now() >= deadline {
            let state = heos.with_player(pid, |p| p.play_state).flatten();
            let refresh = refresh.map_or_else(String::new, |e| format!(", read failed: {e}"));
            return Err(Fail::new(format!(
                "HEOS does not play {HEOS_START_SECS} s after the answer: state {state:?}{refresh}"
            )));
        }
        ctx.client.listen_ms(200).await?;
    }
}

/// The telnet code of the source that HEOS Music selects.
const HEOS_SOURCE: &str = "NET";

/// Pause the HEOS player `pid` if it plays, or stop it if the item cannot pause (for example a
/// station). The player is read again first: the last play event can still be on its way.
/// Returns the note for the check. Fails if the pause or the stop command fails.
async fn end_heos_play(
    heos: &denon_marantz_heos::Heos,
    pid: denon_marantz_heos::PlayerId,
) -> Result<&'static str, Fail> {
    use denon_marantz_heos::PlayState;
    // A failed read is not a failure: the state from the events is used.
    let _ = heos.refresh().await;
    let state = heos.with_player(pid, |p| (p.play_state, p.controls().pause));
    let (state, can_pause) = state.unwrap_or((None, false));
    let Some(send) = heos_end_state(state, can_pause) else {
        return Ok("HEOS did not play at the end, not paused or stopped");
    };
    let (step, note) = if send == PlayState::Stop {
        ("stop", "HEOS stopped at the end: the item cannot pause")
    } else {
        ("pause", "HEOS paused at the end")
    };
    heos.set_play_state(pid, send)
        .await
        .map_err(|e| Fail::new(format!("HEOS {step} at the end failed: {e}")))?;
    Ok(note)
}

/// The play state that [`end_heos_play`] sends for the play state `state`, or `None` if the
/// player does not play. An item that cannot pause (`can_pause`) is stopped.
fn heos_end_state(
    state: Option<denon_marantz_heos::PlayState>,
    can_pause: bool,
) -> Option<denon_marantz_heos::PlayState> {
    use denon_marantz_heos::PlayState;
    match state {
        Some(PlayState::Play) if can_pause => Some(PlayState::Pause),
        Some(PlayState::Play) => Some(PlayState::Stop),
        _ => None,
    }
}

/// Add the note `back` of an end step, for example [`end_heos_play`], to the result of the
/// check. A skip stays a skip. A failed end step fails the check, as in [`first_error`].
fn add_back_note(result: Check, back: Result<impl AsRef<str>, Fail>) -> Check {
    match (result, back) {
        (Ok(note), Ok(back)) => Ok(format!("{note}, {}", back.as_ref())),
        (Err(mut fail), Ok(back)) => {
            let _ = write!(fail.msg, "; {}", back.as_ref());
            Err(fail)
        }
        (result, Err(back)) => first_error(result, Err(back)),
    }
}

/// The position of the second HEOS connection now, in seconds: the last reported one plus the
/// time since then.
fn heos_now(player: &denon_marantz_heos::PlayerState) -> Option<i64> {
    let (progress, at) = (player.progress?, player.progress_at?);
    Some(
        i64::try_from(progress.position_ms / 1000).unwrap_or(i64::MAX)
            + i64::try_from(at.elapsed().as_secs()).unwrap_or(0),
    )
}

/// The position that the Remote shows now: the sent position plus the time since it was sent.
fn heos_position(ctx: &Ctx, mp: &str) -> Result<i64, Fail> {
    let sent = ctx
        .client
        .attr(mp, "media_position")
        .and_then(Value::as_i64)
        .ok_or_else(|| Fail::new("no media_position"))?;
    let at = ctx.client.text(mp, "media_position_updated_at");
    let at = at
        .parse::<jiff::Timestamp>()
        .map_err(|e| Fail::new(format!("media_position_updated_at {at:?}: {e}")))?;
    let since = jiff::Timestamp::now().duration_since(at).as_secs();
    Ok(sent + since)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(result: Result<u8, Fail>) -> (Option<u8>, String, bool) {
        match result {
            Ok(v) => (Some(v), String::new(), false),
            Err(f) => (None, f.msg, f.skip),
        }
    }

    #[test]
    fn first_error_keeps_a_clean_result() {
        assert_eq!(
            check(first_error(Ok(1), Ok(()))),
            (Some(1), String::new(), false)
        );
        assert_eq!(
            check(first_error(Err(Fail::new("body")), Ok(()))),
            (None, "body".to_owned(), false)
        );
        assert_eq!(
            check(first_error(Err(Fail::skip("why")), Ok(()))),
            (None, "why".to_owned(), true),
            "a skip stays a skip"
        );
    }

    #[test]
    fn first_error_reports_a_failed_undo() {
        assert_eq!(
            check(first_error(Ok(1), Err(Fail::new("undo")))),
            (None, "not set back: undo".to_owned(), false)
        );
        assert_eq!(
            check(first_error(Err(Fail::new("body")), Err(Fail::new("undo")))),
            (None, "body; not set back: undo".to_owned(), false),
            "the error of the check comes first"
        );
        assert_eq!(
            check(first_error(Err(Fail::skip("why")), Err(Fail::new("undo")))),
            (None, "why; not set back: undo".to_owned(), false),
            "a skip with a failed undo is a failure"
        );
        assert_eq!(
            check(first_error(Ok(1), Err(Fail::skip("undo")))),
            (None, "not set back: undo".to_owned(), false),
            "a failed undo is never a skip"
        );
    }

    #[test]
    fn lowest_takes_the_lower_known_volume() {
        assert_eq!(lowest(None, None), None);
        assert_eq!(lowest(Some(30.0), None), Some(30.0));
        assert_eq!(lowest(None, Some(30.0)), Some(30.0));
        assert_eq!(lowest(Some(30.0), Some(35.5)), Some(30.0));
        assert_eq!(lowest(Some(35.5), Some(30.0)), Some(30.0));
    }

    #[test]
    fn highest_takes_the_higher_known_volume() {
        assert_eq!(highest(None, None), None);
        assert_eq!(highest(Some(30.0), None), Some(30.0));
        assert_eq!(highest(None, Some(30.0)), Some(30.0));
        assert_eq!(highest(Some(30.0), Some(35.5)), Some(35.5));
        assert_eq!(highest(Some(35.5), Some(30.0)), Some(35.5));
    }

    #[test]
    fn loud_volume_blocks_a_high_or_unknown_volume() {
        assert!(loud_volume(None).is_some());
        assert!(loud_volume(Some(MAX_VOLUME + 0.5)).is_some());
        assert_eq!(loud_volume(Some(MAX_VOLUME)), None);
        assert_eq!(loud_volume(Some(0.0)), None);
        assert!(loud_volume(Some(f64::NAN)).is_some(), "not a number");
    }

    #[test]
    fn heos_is_paused_or_stopped_only_while_it_plays() {
        use denon_marantz_heos::PlayState;
        assert_eq!(
            heos_end_state(Some(PlayState::Play), true),
            Some(PlayState::Pause)
        );
        assert_eq!(
            heos_end_state(Some(PlayState::Play), false),
            Some(PlayState::Stop),
            "an item that cannot pause"
        );
        assert_eq!(heos_end_state(Some(PlayState::Pause), true), None);
        assert_eq!(heos_end_state(Some(PlayState::Stop), false), None);
        assert_eq!(heos_end_state(None, true), None);
    }

    #[test]
    fn source_change_uses_the_label_and_the_known_codes() {
        assert!(!source_changed("TV", "TV", Some("TV"), Some("TV")));
        assert!(source_changed("HEOS Music", "TV", Some("NET"), Some("TV")));
        assert!(
            source_changed("TV", "TV", Some("NET"), Some("TV")),
            "the driver can show an old source"
        );
        assert!(source_changed("HEOS Music", "TV", None, Some("TV")));
        assert!(!source_changed("TV", "TV", None, Some("TV")));
        assert!(!source_changed("TV", "TV", Some("NET"), None));
    }

    #[test]
    fn start_source_must_be_listed_and_allowed() {
        let opt = options(&["oppo"]);
        let sources = [
            source("TV", Some("TV Audio"), None),
            source("BD", Some("Oppo"), None),
            source("NET", Some("HEOS Music"), None),
        ];
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: false,
        };
        let shown = list(&["TV Audio", "Oppo", "HEOS Music", "Source"]);
        assert_eq!(start_source_ok(&rule, "TV Audio", &shown), Ok(()));
        assert_eq!(start_source_ok(&rule, "HEOS Music", &shown), Ok(()));
        assert!(start_source_ok(&rule, "Oppo", &shown).is_err(), "avoided");
        assert!(start_source_ok(&rule, "", &shown).is_err(), "no source");
        assert!(start_source_ok(&rule, "Source", &shown).is_err());
        assert!(
            start_source_ok(&rule, "CD", &shown).is_err(),
            "not in the list"
        );
    }

    #[test]
    fn same_source_compares_what_the_driver_shows() {
        let bd = source("BD", Some("OPPO"), None);
        let mut renamed = source("BD", Some("OPPO"), None);
        renamed.default_name = Some("Blu-ray".to_owned());
        let unnamed = source("TV", None, None);
        assert!(same_source(Some(("BD", Some(&bd))), "", "OPPO"));
        assert!(same_source(Some(("BD", Some(&bd))), "", "OPPO (BD)"));
        assert!(!same_source(Some(("BD", Some(&bd))), "", "OPPO (TV)"));
        assert!(
            same_source(Some(("BD", Some(&bd))), "Apple TV", "OPPO"),
            "the receiver decides, the driver can be late"
        );
        assert!(
            !same_source(Some(("BD", Some(&renamed))), "", "Blu-ray"),
            "the driver does not show the default name"
        );
        assert!(same_source(Some(("TV", Some(&unnamed))), "", "TV"));
        assert!(
            !same_source(Some(("BD", Some(&bd))), "", ""),
            "no start source"
        );
        assert!(same_source(None, "OPPO", "OPPO"));
        assert!(!same_source(None, "", ""));
        assert!(!same_source(None, "Apple TV", "OPPO"));
        assert!(!same_source(Some(("GAME", None)), "", "OPPO"));
    }

    #[test]
    fn now_ok_follows_the_source_rule() {
        let opt = options(&["oppo"]);
        let sources = [
            source("MPLAY", Some("Apple TV"), None),
            source("BD", Some("OPPO"), None),
            source("GAME", Some("Same"), None),
            source("TV", Some("Same"), None),
        ];
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: false,
        };
        assert_eq!(now_ok(&rule, None, "Apple TV"), Ok(()));
        assert_eq!(
            now_ok(&rule, None, "OPPO"),
            Err("the source OPPO may not be changed".to_owned())
        );
        assert_eq!(
            now_ok(&rule, None, ""),
            Err("the source (unknown) may not be changed".to_owned())
        );
        // The code decides: the driver can show an old source.
        assert_eq!(now_ok(&rule, Some("MPLAY"), "OPPO"), Ok(()));
        assert_eq!(
            now_ok(&rule, Some("BD"), "Apple TV"),
            Err("the source BD may not be changed".to_owned())
        );
        // Two sources with the same name: only the code can tell them apart.
        assert!(now_ok(&rule, None, "Same").is_err());
        assert_eq!(now_ok(&rule, Some("GAME"), "Same"), Ok(()));
        // A code that the receiver does not list.
        assert_eq!(
            now_ok(&rule, Some("NET"), "HEOS Music"),
            Err("the source NET may not be changed".to_owned())
        );
    }

    #[test]
    fn skipped_matches_reset_only_as_a_whole_part() {
        assert!(skipped("CHANNEL_VOLUMES_RESET"));
        assert!(skipped("RESET"));
        assert!(skipped("TUNER_PRESET_UP"), "the frequency is not set back");
        assert!(skipped("TUNER_PRESET_DOWN"));
        assert!(!skipped("SPEAKER_PRESET_1"));
        assert!(!skipped("SPEAKER_PRESET_TOGGLE"));
        assert!(!skipped("AUROMATIC_3D_PRESET_SMALL"));
        assert!(skipped("QUICK_SELECT_1"));
        assert!(skipped("VOLUME_UP"));
    }

    #[test]
    fn the_presets_that_the_sweep_sends_are_set_back() {
        for line in COMMANDS.lines() {
            let Some((name, _, text)) = command_fields(line) else {
                continue;
            };
            if name.contains("PRESET") && !skipped(name) {
                let code = text.split(' ').next().unwrap_or_default();
                assert!(
                    restore::QUERIES
                        .iter()
                        .any(|q| q.trim_end_matches('?').trim_end() == code),
                    "{name} ({text}) has no query in restore::QUERIES"
                );
            }
        }
    }

    #[test]
    fn add_back_note_keeps_the_result_and_the_skip() {
        let paused = "HEOS paused at the end";
        assert_eq!(
            add_back_note(Ok("done".to_owned()), Ok(paused)).ok(),
            Some(format!("done, {paused}"))
        );
        let fail = add_back_note(Err(Fail::skip("why")), Ok(paused)).unwrap_err();
        assert_eq!((fail.msg, fail.skip), (format!("why; {paused}"), true));
        let fail = add_back_note(Err(Fail::new("body")), Ok(paused.to_owned())).unwrap_err();
        assert_eq!((fail.msg, fail.skip), (format!("body; {paused}"), false));
        let fail =
            add_back_note(Ok("done".to_owned()), Err::<&str, _>(Fail::new("pause"))).unwrap_err();
        assert_eq!(
            (fail.msg, fail.skip),
            ("not set back: pause".to_owned(), false)
        );
        let fail =
            add_back_note(Err(Fail::skip("why")), Err::<&str, _>(Fail::new("pause"))).unwrap_err();
        assert_eq!(
            (fail.msg, fail.skip),
            ("why; not set back: pause".to_owned(), false)
        );
    }

    #[test]
    fn add_set_back_skips_a_passed_check_if_the_owner_left_the_source() {
        let skipped = |note: &str| {
            Ok(SetBack {
                note: note.to_owned(),
                skipped: true,
            })
        };
        let note = |note: &str| Ok(SetBack::note(note.to_owned()));
        assert_eq!(
            add_set_back(Ok("done".to_owned()), note("set back")).ok(),
            Some("done, set back".to_owned())
        );
        assert_eq!(
            add_set_back(Ok("done".to_owned()), note("")).ok(),
            Some("done".to_owned()),
            "the source did not change"
        );
        let fail = add_set_back(Ok("done".to_owned()), skipped("left")).unwrap_err();
        assert_eq!((fail.msg, fail.skip), ("done, left".to_owned(), true));
        let fail = add_set_back(Err(Fail::new("body")), skipped("left")).unwrap_err();
        assert_eq!((fail.msg, fail.skip), ("body; left".to_owned(), false));
        let fail = add_set_back(Err(Fail::skip("why")), skipped("left")).unwrap_err();
        assert_eq!((fail.msg, fail.skip), ("why; left".to_owned(), true));
        let fail = add_set_back(Err(Fail::skip("why")), Err(Fail::new("back"))).unwrap_err();
        assert_eq!(
            (fail.msg, fail.skip),
            ("why; not set back: back".to_owned(), false)
        );
    }

    #[test]
    fn back_volume_ok_needs_a_known_volume_at_or_below_the_start() {
        assert_eq!(back_volume_ok(Some(30.0), 30.0), Ok(()));
        assert_eq!(back_volume_ok(Some(20.0), 30.0), Ok(()));
        assert!(back_volume_ok(Some(30.5), 30.0).is_err(), "above the start");
        assert!(back_volume_ok(None, 30.0).is_err(), "not known");
        assert!(
            back_volume_ok(Some(MAX_VOLUME + 0.5), 60.0).is_err(),
            "above the limit"
        );
        assert_eq!(back_volume_ok(Some(MAX_VOLUME), 60.0), Ok(()));
        assert!(
            back_volume_ok(Some(f64::NAN), 30.0).is_err(),
            "not a number"
        );
    }

    #[test]
    fn sound_volume_gives_the_volume_or_why_not() {
        assert_eq!(sound_volume(Some(MAX_VOLUME)), Ok(MAX_VOLUME));
        assert!(sound_volume(Some(MAX_VOLUME + 0.5)).is_err());
        assert!(sound_volume(None).is_err());
    }

    use crate::tests::options;

    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn source(code: &str, name: Option<&str>, auto_name: Option<&str>) -> SourceInfo {
        SourceInfo {
            code: code.to_owned(),
            name: name.map(str::to_owned),
            in_use: Some(true),
            default_name: Some(code.to_owned()),
            auto_name: auto_name.map(str::to_owned),
            hdmi_input: None,
            digital_input: None,
            analog_input: None,
            video_input: None,
        }
    }

    #[test]
    fn a_source_without_media_shows_its_name_as_the_title() {
        let sources = [
            source("MPLAY", Some("Apple TV"), None),
            source("NET", Some("HEOS Music"), None),
            source("TUNER", None, None),
            source("GAME", Some("TV"), None),
            source("SAT/CBL", Some("TV"), None),
        ];
        assert_eq!(Some("Apple TV"), source_title(&sources, "Apple TV"));
        assert_eq!(Some("TV (GAME)"), source_title(&sources, "TV (GAME)"));
        assert_eq!(None, source_title(&sources, "HEOS Music"), "network audio");
        assert_eq!(None, source_title(&sources, "TUNER"), "tuner");
        assert_eq!(None, source_title(&sources, "TV"), "two sources");
        assert_eq!(None, source_title(&sources, "Blu-ray"), "no source");
    }

    #[test]
    fn signal_time_as_the_setup_page_shows_it() {
        let secs = |v: Value| signal_secs(&json!({ "signal_poll_secs": v }));
        assert_eq!(DEFAULT_SIGNAL_SECS, signal_secs(&json!({})));
        assert_eq!(DEFAULT_SIGNAL_SECS, secs(Value::Null));
        assert_eq!(0, secs(json!(0)));
        assert_eq!(MIN_SIGNAL_SECS, secs(json!(1)));
        assert_eq!(10, secs(json!(10)));
        assert_eq!(MAX_SIGNAL_SECS, secs(json!(60000)));
        assert_eq!(MAX_SIGNAL_SECS, secs(json!(100_000)));
    }

    #[test]
    fn bass_changes_only_with_tone_control_on_and_dirac_off() {
        assert_eq!(None, bass_skip("ON", Some(true)));
        assert!(bass_skip("OFF", Some(true)).is_some());
        assert!(bass_skip("UNKNOWN", Some(true)).is_some());
        assert!(bass_skip("not offered", Some(true)).is_some());
        assert!(bass_skip("ON", Some(false)).is_some());
        assert!(bass_skip("ON", None).is_some());
    }

    #[test]
    fn dirac_off_prefers_the_receiver() {
        let off = DiracFilter::Off.label();
        assert_eq!(
            Some(true),
            dirac_off(Some(DiracFilter::Off), Some("Slot 1"), true)
        );
        assert_eq!(
            Some(false),
            dirac_off(Some(DiracFilter::Slot2), Some(off), true)
        );
        assert_eq!(Some(true), dirac_off(None, Some(off), true));
        assert_eq!(Some(false), dirac_off(None, Some("Cinema"), true));
        assert_eq!(None, dirac_off(None, Some("--"), true));
        assert_eq!(None, dirac_off(None, Some(""), true));
        assert_eq!(None, dirac_off(None, None, true), "no select, Dirac set up");
        assert_eq!(Some(true), dirac_off(None, None, false), "no Dirac");
    }

    #[test]
    fn bass_goes_one_step_lower_within_the_range() {
        assert_eq!(Some(-1), bass_lower(0, 1, -6));
        assert_eq!(Some(-6), bass_lower(-5, 1, -6));
        assert_eq!(None, bass_lower(-6, 1, -6));
        assert_eq!(Some(-4), bass_lower(-2, 2, -6));
        assert_eq!(None, bass_lower(-5, 2, -6));
        assert_eq!(None, bass_lower(i64::MIN, 1, i64::MIN));
    }

    #[test]
    fn a_source_may_be_selected_only_with_a_name_that_is_not_avoided() {
        let opt = options(&["oppo"]);
        let tv = source("MPLAY", Some("Apple TV"), None);
        assert!(may_send_source(&opt, &tv, false));
        assert!(
            !may_send_source(&options(&[]), &tv, false),
            "no --avoid-sources"
        );
        let renamed = source("BD", Some("OPPO"), None);
        assert!(!may_send_source(&opt, &renamed, false));
        let auto = source("BD", Some("Blu-ray"), Some("Oppo UDP-203"));
        assert!(!may_send_source(&opt, &auto, false), "auto name");
        let default = SourceInfo {
            default_name: Some("Oppo".to_owned()),
            ..source("BD", Some("Blu-ray"), None)
        };
        assert!(!may_send_source(&opt, &default, false), "default name");
        assert!(
            !may_send_source(&options(&["bd"]), &auto, false),
            "telnet code"
        );
        let no_name = source("CD", None, None);
        assert!(!may_send_source(&opt, &no_name, false), "name not known");
        let not_on_page = SourceInfo {
            default_name: None,
            ..source("DVD", Some("DVD"), None)
        };
        assert!(
            !may_send_source(&opt, &not_on_page, false),
            "not on the names page"
        );
        let hidden = SourceInfo {
            in_use: Some(false),
            ..source("CD", Some("CD"), None)
        };
        assert!(!may_send_source(&opt, &hidden, false));
        let unknown = SourceInfo {
            in_use: None,
            ..source("CD", Some("CD"), None)
        };
        assert!(!may_send_source(&opt, &unknown, false), "in use not known");
        for code in TUNER_CODES {
            let tuner = source(code, Some("Radio"), None);
            assert!(!may_send_source(&opt, &tuner, false), "{code}");
            assert!(may_send_source(&opt, &tuner, true), "{code}");
        }
    }

    #[test]
    fn debug_lines_count_only_the_level() {
        let log = "[2026-11-26T10:00:00.123Z DEBUG uc_intg_denonmarantz] [A] <- {}\n\
            [2026-11-26T10:00:00.124Z INFO  uc_intg_denonmarantz] title: A DEBUG song\n\
            DEBUG at the start\n\
            [2026-11-26T10:00:00.125Z DEBUG denon_marantz] connected\n";
        assert_eq!(2, debug_lines(log));
        assert_eq!(0, debug_lines(""));
    }

    #[test]
    fn panic_line_finds_the_panic() {
        assert_eq!(None, panic_line(""));
        assert_eq!(None, panic_line("[ts INFO x] [A] connected in 20 ms\n"));
        assert_eq!(
            None,
            panic_line("[ts DEBUG x] [A] title: Everybody panicked at once\n"),
            "receiver text"
        );
        assert_eq!(
            None,
            panic_line("[ts DEBUG x] [A] title: 'Tom' panicked at noon\n"),
            "receiver text with a quote"
        );
        assert_eq!(
            None,
            panic_line("[ts DEBUG x] [A] title: thread 'a' panicked at b\n"),
            "receiver text with the whole panic text"
        );
        assert_eq!(
            Some("thread '<unnamed>' (7) panicked at src/x.rs:1:2: boom".to_owned()),
            panic_line("thread '<unnamed>' (7) panicked at src/x.rs:1:2:\nboom\n"),
            "thread without a name"
        );
        assert_eq!(
            Some("thread 'main' panicked at src/x.rs:1:2:".to_owned()),
            panic_line("thread 'main' panicked at src/x.rs:1:2:"),
            "format without a thread id, no message line"
        );
        let log = "[ts INFO x] start\n\
                   \n\
                   thread 'actix-rt|system:0|arbiter:0' (15243207) panicked at src/x.rs:1:2:\n\
                   boom\n\
                   note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert_eq!(
            Some(
                "thread 'actix-rt|system:0|arbiter:0' (15243207) panicked at src/x.rs:1:2: boom"
                    .to_owned()
            ),
            panic_line(log)
        );
    }

    #[test]
    fn abort_line_matches_the_driver_log() {
        let src = include_str!("../../../src/controller/mod.rs");
        assert!(src.contains(r#"info!("State machine transition: {old_state:?} -> {state:?}")"#));
        let (from, to) = ABORT_LINE
            .strip_prefix("State machine transition: ")
            .and_then(|states| states.split_once(" -> "))
            .expect("two states");
        let block = src
            .split_once(&format!("    {from} => {{"))
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(block, _)| block)
            .expect("state of ABORT_LINE");
        assert!(block.contains(&format!("AbortSetup => {to}")));
        let setup = include_str!("../../../src/controller/handler/setup.rs");
        assert!(
            setup.contains("operation_mode::Input::AbortSetup"),
            "a user abort uses the AbortSetup input"
        );
        let event = include_str!("../../../src/controller/handler/r2_event.rs");
        let abort = event
            .split_once("R2Event::AbortDriverSetup =>")
            .map(|(_, rest)| rest.split_once("R2Event::").map_or(rest, |(arm, _)| arm))
            .expect("abort event arm");
        assert!(
            abort.contains("timeout: false"),
            "the abort event is not a timeout"
        );
    }

    #[test]
    fn session_end_finds_the_lines_of_one_session() {
        let id = "[127.0.0.1:5000]";
        assert_eq!((false, false), session_end("", id));
        let log = "[ts INFO uc_intg_denonmarantz::server::ws::connection] [127.0.0.1:5000] \
                   Remote closed connection. Reason: Some(CloseReason { code: Other(4001), \
                   description: Some(\"bye\") })\n\
                   [ts DEBUG uc_intg_denonmarantz::server::ws::connection] [127.0.0.1:5000] stopped\n";
        assert_eq!((true, true), session_end(log, id));
        assert_eq!(
            (false, false),
            session_end(log, "[127.0.0.1:500]"),
            "another port"
        );
        let other = "[ts DEBUG x] [127.0.0.1:50001] stopped\n\
                     [ts DEBUG x] [127.0.0.1:5000] stopped later\n\
                     [ts DEBUG x] [A] title: [127.0.0.1:5000] Remote closed connection\n";
        assert_eq!((false, false), session_end(other, id));
        assert_eq!(
            (false, true),
            session_end("[ts DEBUG x] [127.0.0.1:5000] stopped\n", id)
        );
    }

    #[test]
    fn session_end_lines_match_the_driver_log() {
        let src = include_str!("../../../src/server/ws/connection.rs");
        assert!(src.contains(r#"debug!("[{}] stopped", self.id);"#));
        assert!(
            src.contains(r#"info!("[{}] Remote closed connection. Reason: {reason:?}", self.id);"#)
        );
        let index = include_str!("../../../src/server/ws/mod.rs");
        assert!(index.contains("|addr| addr.to_string()"), "id is ip:port");
    }

    #[test]
    fn idle_time_is_longer_than_the_driver_timeout() {
        let src = include_str!("../../../src/configuration.rs");
        assert!(src.contains("interval: Duration::from_secs(20),"));
        assert!(src.contains("timeout: Duration::from_secs(40),"));
        const { assert!(IDLE_SECS > 40) };
    }

    #[test]
    fn same_ids_lists_missing_and_new_ids() {
        let set =
            |ids: &[&str]| -> BTreeSet<String> { ids.iter().map(|&i| i.to_owned()).collect() };
        assert!(same_ids(&set(&["a", "b"]), &set(&["a", "b"]), "now").is_ok());
        let e = same_ids(&set(&["a", "b"]), &set(&["a", "c"]), "now").expect_err("differs");
        assert_eq!(e.msg, r#"entities now: missing ["b"], new ["c"]"#);
    }

    #[test]
    fn unnamed_finds_entities_without_an_english_name() {
        let list = [
            json!({ "entity_id": "a", "name": { "en": "A" } }),
            json!({ "entity_id": "b", "name": { "en": "" } }),
            json!({ "entity_id": "c", "name": { "de": "C" } }),
            json!({ "entity_id": "d" }),
            json!({ "name": {} }),
        ];
        assert_eq!(vec!["b", "c", "d", "?"], unnamed(&list));
    }

    #[test]
    fn restart_wait_follows_the_main_zone_power() {
        assert_eq!(ShownPower::On, restart_wait(Some(Power::On)));
        assert_eq!(ShownPower::Off, restart_wait(Some(Power::Standby)));
        assert_eq!(ShownPower::Off, restart_wait(Some(Power::Off)));
        assert_eq!(ShownPower::Unknown, restart_wait(None));
    }

    #[test]
    fn invalid_ids_lists_bad_and_missing_ids() {
        let list = [
            json!({ "entity_id": "media_player.A1" }),
            json!({ "entity_id": "sensor.a b" }),
            json!({ "name": { "en": "X" } }),
        ];
        assert_eq!(vec!["sensor.a b", "?"], invalid_ids(&list));
    }

    #[test]
    fn device_choice_takes_the_address_or_the_only_item() {
        let page = |ids: &[&str]| {
            let items: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
            json!({ "settings": [
                { "id": "choice", "field": { "dropdown": { "value": ids.first(), "items": items } } }
            ] })
        };
        let one = page(&["192.0.2.1"]);
        assert_eq!(
            Some("192.0.2.1".to_owned()),
            device_choice(&one, "192.0.2.1")
        );
        assert_eq!(
            Some("192.0.2.1".to_owned()),
            device_choice(&one, "avr.local"),
            "host name"
        );
        let two = page(&["192.0.2.1", "192.0.2.2"]);
        assert_eq!(
            Some("192.0.2.2".to_owned()),
            device_choice(&two, "192.0.2.2")
        );
        assert_eq!(None, device_choice(&two, "avr.local"), "not known");
        assert_eq!(None, device_choice(&page(&[]), "avr.local"));
        assert_eq!(None, device_choice(&json!({}), "avr.local"));
    }

    #[test]
    fn valid_entity_id_allows_only_safe_characters() {
        assert!(valid_entity_id("media_player.0123ABC"));
        assert!(valid_entity_id("select.input_source.AB-12_c"));
        assert!(valid_entity_id(&format!("sensor.{}", "a".repeat(60))));
        assert!(!valid_entity_id(""));
        assert!(!valid_entity_id("media player.1"));
        assert!(!valid_entity_id("sensor.vol/1"));
        assert!(!valid_entity_id("sensor.völ"));
    }

    #[test]
    fn quick_gaps_cover_both_races() {
        assert!(
            QUICK_GAPS_MS.iter().any(|&(awake, _)| awake < 30),
            "a standby while a connect runs"
        );
        assert!(
            QUICK_GAPS_MS
                .iter()
                .any(|&(awake, asleep)| awake >= 100 && asleep == 0),
            "a wake-up while the old link closes"
        );
    }

    #[test]
    fn woke_needs_the_connected_status() {
        assert!(woke(None, Some("Connected")));
        assert!(woke(Some("Disconnected"), Some("Connected")));
        assert!(woke(Some("Connected"), None), "the driver stayed connected");
        assert!(!woke(Some("Connected"), Some("Disconnected")));
        assert!(!woke(Some("Connected"), Some("Connecting")));
        assert!(!woke(Some("Disconnected"), None));
        assert!(!woke(None, None));
    }

    #[test]
    fn last_status_reads_the_last_status_line() {
        assert_eq!(None, last_status(""));
        assert_eq!(None, last_status("[A] connected in 20 ms\n"));
        let log = "[A] status Disconnected -> Connecting\n\
                   [A] status Connecting -> Connected\n\
                   [A] connected in 20 ms\n";
        assert_eq!(Some("Connected"), last_status(log));
        let log = format!("{log}[A] status Connected -> Disconnected\r\n");
        assert_eq!(Some("Disconnected"), last_status(&log));
        let log = "[2026-11-26T10:00:00.000Z DEBUG uc_intg_denonmarantz::device::actor] \
                   [A] status Disconnected -> Connecting\n\
                   [2026-11-26T10:00:00.050Z DEBUG denon_marantz::avr] status page not available\n";
        assert_eq!(
            Some("Connecting"),
            last_status(log),
            "a line without -> does not count"
        );
        let log = format!("{log}[ws1] -> {{\"kind\":\"event\"}}\n");
        assert_eq!(
            Some("Connecting"),
            last_status(&log),
            "a WebSocket line does not count"
        );
        let log = format!("{log}[A] status Connecting -> Error\n");
        assert_eq!(Some("Error"), last_status(&log));
        assert_eq!(None, new_status("[A] status Connected -> Foo"));
        assert_eq!(
            Some("Connected"),
            last_status("[A] status Connecting -> Connected\n[A] status Connected -> Disconn"),
            "a cut last line does not count"
        );
    }

    #[test]
    fn status_count_counts_one_new_status() {
        let log = "[A] status Disconnected -> Connecting\n\
                   [A] status Connecting -> Connected\n\
                   [A] status Connected -> Disconnected\n\
                   [A] status Disconnected -> Connected\r\n\
                   [ws1] -> Connected\n";
        assert_eq!(2, status_count(log, "Connected"));
        assert_eq!(1, status_count(log, "Disconnected"));
        assert_eq!(0, status_count(log, "Error"));
        assert_eq!(0, status_count("", "Connected"));
        let log = "[A] address changed: 1 -> 2\n\
                   [A] status page failed: a -> b\n\
                   [A -> B] status Connecting -> Connected\n";
        assert_eq!(1, status_count(log, "Connected"));
        assert_eq!(Some("Connected"), last_status(log));
        assert_eq!(None, new_status("[A] status page failed: a -> b"));
    }

    #[test]
    fn connect_ms_reads_the_last_connect() {
        assert_eq!(None, connect_ms(""));
        assert_eq!(None, connect_ms("[A] connected in ? ms\n"));
        let log = "[A] connected in 20 ms\n\
                   [A] connected in 45 ms\n";
        assert_eq!(Some(45), connect_ms(log));
    }

    #[test]
    fn after_wake_needs_one_connect_and_no_drop() {
        let one = "[A] status Disconnected -> Connecting\n\
                   [A] status Connecting -> Connected\n";
        assert_eq!(Ok(0), after_wake(one, 0, false));
        assert_eq!(
            Err("connects failed: 1".to_owned()),
            after_wake(one, 1, false)
        );
        assert!(after_wake("", 0, false).is_err(), "no connect");
        let dropped = format!(
            "{one}[A] status Connected -> Disconnected\n\
             [A] status Disconnected -> Connecting\n\
             [A] status Connecting -> Connected\n"
        );
        assert_eq!(
            Err("after the last exit_standby: 2 connects, 1 drops".to_owned()),
            after_wake(&dropped, 0, false)
        );
        let error = format!("{one}[A] status Connected -> Error\n");
        assert!(after_wake(&error, 0, false).is_err(), "error status");
        let retry = format!("{one}[A] telnet not open, trying again: refused\n");
        assert_eq!(
            Err("telnet not open after the last exit_standby".to_owned()),
            after_wake(&retry, 0, true)
        );
        let open = format!("{retry}[A] telnet connected\n");
        assert_eq!(Ok(1), after_wake(&open, 0, true));
        let lib = format!("{retry}[ts DEBUG denon_marantz::telnet] telnet connected host=h\n");
        assert_eq!(Ok(1), after_wake(&lib, 0, true), "the library line counts");
        let lost = format!("{open}[A] telnet disconnected\n");
        assert_eq!(
            Err("telnet link lost after the last exit_standby".to_owned()),
            after_wake(&lost, 0, true)
        );
        let lib_lost = format!(
            "{open}[ts DEBUG denon_marantz::telnet] telnet connection lost host=h \
             reason=closed by the receiver\n"
        );
        assert_eq!(
            Err("telnet link lost after the last exit_standby".to_owned()),
            after_wake(&lib_lost, 0, true)
        );
        let again = format!("{open}[A] telnet not open, trying again: refused\n");
        assert!(
            after_wake(&again, 0, true).is_err(),
            "only a later open counts"
        );
        assert_eq!(
            Err("telnet not connected after the last exit_standby".to_owned()),
            after_wake(one, 0, true)
        );
        let started = format!("{one}[ts DEBUG denon_marantz::telnet] telnet connected host=h\n");
        assert_eq!(Ok(0), after_wake(&started, 0, true));
    }

    #[test]
    fn failed_connects_counts_only_receiver_connects() {
        let log = "[A] connect failed: timeout\n\
                   [A] not found on the network\n\
                   [A] HEOS connect failed: refused\n\
                   telnet reconnect failed\n\
                   HEOS reconnect failed\n\
                   [A] connect with the stored values failed: timeout\n";
        assert_eq!(2, failed_connects(log));
        assert_eq!(0, failed_connects(""));
    }

    #[test]
    fn canceled_connects_counts_cancel_lines() {
        let log = "[A] connect canceled\n\
                   [A] connect canceled\n\
                   [A] result of an old connect attempt ignored\n";
        assert_eq!(2, canceled_connects(log));
        assert_eq!(0, canceled_connects(""));
    }

    #[test]
    fn only_a_connect_after_the_last_loss_counts() {
        let old = "[A] connected in 20 ms\n";
        assert_eq!(None, last_loss(old));
        let lost = format!("{old}telnet connection lost\n");
        let at = last_loss(&lost).expect("lost");
        assert!(
            !connected_after(&lost, at),
            "the old connect is before the loss"
        );
        let again = format!("{lost}[A] telnet disconnected\ntelnet connected\n");
        let at = last_loss(&again).expect("lost");
        assert!(at > lost.len(), "the last loss line");
        assert!(connected_after(&again, at));
        assert!(
            !connected_after("[A] telnet disconnected\n", 0),
            "not a connect line"
        );
        assert!(!connected_after(old, old.len() + 1), "past the end");
    }

    #[test]
    fn a_driver_label_fits_one_source() {
        let bd = source("BD", Some("Blu-ray"), None);
        assert!(label_fits(&bd, "Blu-ray"));
        assert!(label_fits(&bd, "Blu-ray (BD)"));
        assert!(label_fits(&bd, "BD"));
        assert!(!label_fits(&bd, "Blu-ray (DVD)"));
        assert!(!label_fits(&bd, "Blu"));
        let opt = options(&["oppo"]);
        let sources = [bd, source("DVD", Some("Blu-ray"), None)];
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: false,
        };
        assert!(!rule.label_ok("Blu-ray"), "two sources have the name");
        assert!(rule.label_ok("Blu-ray (DVD)"));
        assert!(!rule.label_ok("Apple TV"), "not a source of the receiver");
    }

    fn receiver_sources() -> Vec<SourceInfo> {
        vec![
            source("BD", Some("OPPO"), None),
            source("TV", Some("TV Audio"), None),
            source("MPLAY", Some("Apple TV"), None),
        ]
    }

    #[test]
    fn other_source_is_one_that_may_be_selected() {
        let opt = options(&["oppo"]);
        let sources = receiver_sources();
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: false,
        };
        let shown = list(&["Source", "OPPO", "TV Audio", "Apple TV"]);
        assert_eq!(
            other_source(&rule, "TV Audio", &shown),
            Ok("Apple TV".to_owned())
        );
        assert_eq!(
            other_source(&rule, "Apple TV", &shown),
            Ok("TV Audio".to_owned())
        );
        let two = list(&["Source", "OPPO", "Apple TV"]);
        assert!(
            other_source(&rule, "Apple TV", &two).is_err(),
            "only one source"
        );
        // The code counts too: `BD` avoids the source shown as `OPPO`.
        let by_code = options(&["bd", "tv audio"]);
        let rule = SourceRule {
            opt: &by_code,
            ..rule
        };
        let shown = list(&["OPPO", "Apple TV", "TV Audio"]);
        assert!(other_source(&rule, "Apple TV", &shown).is_err());
    }

    #[test]
    fn other_source_needs_a_start_source_that_can_be_set_back() {
        let shown = list(&["Source", "OPPO", "TV Audio", "Apple TV"]);
        let sources = receiver_sources();
        let opt = options(&["oppo"]);
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: false,
        };
        let none = options(&[]);
        let no_avoid = SourceRule { opt: &none, ..rule };
        assert!(other_source(&no_avoid, "TV Audio", &shown).is_err());
        assert!(other_source(&rule, "", &shown).is_err(), "no start source");
        assert!(other_source(&rule, "Source", &shown).is_err());
        assert!(
            other_source(&rule, "CD", &shown).is_err(),
            "not in the list"
        );
        assert!(
            other_source(&rule, "OPPO", &shown).is_err(),
            "the set back would select an avoided source"
        );
    }

    #[test]
    fn sources_are_selected_only_without_added_input_levels() {
        let levels = |analog_db, digital_db| SourceLevels {
            analog_db,
            digital_db,
        };
        assert!(levels_ok(&levels(Some(0), Some(0))).is_ok());
        assert!(levels_ok(&levels(Some(-12), Some(-3))).is_ok());
        assert!(levels_ok(&levels(Some(1), Some(0))).is_err());
        assert!(levels_ok(&levels(Some(0), Some(12))).is_err());
        assert!(levels_ok(&levels(None, Some(0))).is_err(), "not known");
        assert!(levels_ok(&levels(Some(0), None)).is_err(), "not known");
    }

    #[test]
    fn source_and_tuner_commands_are_sent_last() {
        let codes = [("INPUT_TV", "TV")];
        let mut names = vec!["INPUT_TV", "MUTE_ON", "TUNER_BAND_FM", "SLEEP_OFF"];
        names.sort_by_key(|n| changes_source(n, &codes));
        assert_eq!(
            vec!["MUTE_ON", "SLEEP_OFF", "INPUT_TV", "TUNER_BAND_FM"],
            names
        );
    }

    #[test]
    fn source_and_tuner_commands_follow_the_start_source() {
        let opt = options(&["oppo"]);
        let mut sources = receiver_sources();
        sources.push(source("TUNER", Some("Radio"), None));
        let rule = SourceRule {
            opt: &opt,
            sources: &sources,
            tuner: true,
        };
        let plan = SourcePlan::new(&rule, Some("MPLAY"));
        assert!(plan.command_ok("INPUT_TV", &["TV"]));
        assert!(!plan.command_ok("INPUT_BD", &["BD"]), "avoided");
        assert!(!plan.command_ok("INPUT_CD", &["CD"]), "not a source");
        assert!(
            !plan.command_ok("INPUT_X", &["TV", "BD"]),
            "one code avoided"
        );
        assert!(plan.command_ok("TUNER_BAND_FM", &[]));
        assert!(plan.command_ok("SLEEP_OFF", &[]));

        let avoided_start = SourcePlan::new(&rule, Some("BD"));
        assert!(!avoided_start.start_ok);
        assert!(avoided_start.why.contains("BD"), "{}", avoided_start.why);
        assert!(!avoided_start.command_ok("INPUT_TV", &["TV"]));
        assert!(!avoided_start.command_ok("TUNER_BAND_FM", &[]));
        assert!(avoided_start.command_ok("SLEEP_OFF", &[]));
        assert!(!SourcePlan::new(&rule, None).start_ok, "unknown start");
        assert!(!SourcePlan::new(&rule, Some("CD")).start_ok, "not known");

        let radio = options(&["oppo", "radio"]);
        let rule = SourceRule {
            opt: &radio,
            ..rule
        };
        let plan = SourcePlan::new(&rule, Some("MPLAY"));
        assert!(plan.start_ok);
        assert!(!plan.command_ok("TUNER_BAND_FM", &[]), "tuner avoided");
        let no_tuner = &sources[..3];
        let rule = SourceRule {
            opt: &opt,
            sources: no_tuner,
            tuner: false,
        };
        let plan = SourcePlan::new(&rule, Some("MPLAY"));
        assert!(!plan.command_ok("TUNER_BAND_FM", &[]), "no tuner source");

        let none = SourcePlan::none("no --avoid-sources".to_owned());
        assert!(!none.command_ok("INPUT_TV", &["TV"]));
        assert!(!none.command_ok("TUNER_BAND_FM", &[]));
        assert!(none.command_ok("SLEEP_OFF", &[]));
    }

    #[test]
    fn a_volume_is_sent_only_when_the_receiver_is_above_the_limit() {
        assert_eq!(volume_to_send(45.0, 40.0), Some(40.0));
        assert_eq!(volume_to_send(40.5, 40.0), Some(40.0));
        assert_eq!(volume_to_send(40.0, 40.0), None);
        assert_eq!(volume_to_send(20.0, 40.0), None, "never up to the limit");
    }

    #[test]
    fn the_volume_check_sets_back_only_its_own_changes() {
        assert_eq!(volume_back_target(30.0, 30.0), None, "already at the start");
        assert_eq!(
            volume_back_target(28.0, 30.0),
            Some(30.0),
            "the lowest step"
        );
        assert_eq!(volume_back_target(29.0, 30.0), Some(30.0));
        assert_eq!(
            volume_back_target(31.0, 30.0),
            Some(30.0),
            "above the start"
        );
        assert_eq!(
            volume_back_target(27.5, 30.0),
            None,
            "set lower by someone else"
        );
    }

    #[test]
    fn above_baseline_uses_the_start_and_the_limit() {
        assert!(above_baseline(30.5, Some(30.0)));
        assert!(!above_baseline(30.0, Some(30.0)));
        assert!(!above_baseline(29.0, Some(30.0)));
        assert!(above_baseline(40.5, Some(55.0)), "the limit counts");
        assert!(above_baseline(40.5, None));
        assert!(!above_baseline(40.0, None));
    }

    #[test]
    fn other_sound_needs_another_receiver_mode() {
        let modes = ("DOLBY SURROUND".to_owned(), "STEREO".to_owned());
        assert!(other_sound("MULTI CH IN", Some(&modes)));
        assert!(!other_sound("Stereo", Some(&modes)));
        assert!(!other_sound("dolby surround", Some(&modes)));
        assert!(!other_sound("", Some(&modes)));
        assert!(!other_sound("MULTI CH IN", None));
    }

    #[test]
    fn join_errors_keeps_all_errors() {
        let msg = |r: Result<(), Fail>| r.err().map(|f| (f.msg, f.skip));
        assert_eq!(msg(join_errors(Ok(()), Ok(()))), None);
        assert_eq!(
            msg(join_errors(Err(Fail::new("a")), Ok(()))),
            Some(("a".to_owned(), false))
        );
        assert_eq!(
            msg(join_errors(Ok(()), Err(Fail::new("b")))),
            Some(("b".to_owned(), false))
        );
        assert_eq!(
            msg(join_errors(Err(Fail::new("a")), Err(Fail::skip("b")))),
            Some(("a; b".to_owned(), false))
        );
    }

    #[test]
    fn unmute_limit_is_never_above_the_start_or_the_limit() {
        assert_eq!(unmute_limit(None), MAX_VOLUME);
        assert_eq!(unmute_limit(Some(30.0)), 30.0);
        assert_eq!(unmute_limit(Some(MAX_VOLUME)), MAX_VOLUME);
        assert_eq!(unmute_limit(Some(55.5)), MAX_VOLUME);
        assert_eq!(unmute_limit(Some(35.5)), 35.5, "a half step stays");
        assert_eq!(unmute_limit(Some(0.5)), 0.5);
    }

    #[test]
    fn volume_plan_never_sends_above_the_limit() {
        assert_eq!(
            volume_plan(35.5).ok(),
            Some(VolumePlan::Raw {
                command: "MV355".to_owned(),
                fallback: 35.0
            })
        );
        assert_eq!(volume_plan(35.0).ok(), Some(VolumePlan::Volume(35.0)));
        assert_eq!(
            volume_plan(0.5).ok(),
            Some(VolumePlan::Raw {
                command: "MV005".to_owned(),
                fallback: 0.0
            })
        );
        assert_eq!(volume_plan(0.0).ok(), Some(VolumePlan::Volume(0.0)));
        assert_eq!(
            volume_plan(35.25).ok(),
            Some(VolumePlan::Volume(35.0)),
            "the next lower whole number"
        );
        assert_eq!(volume_plan(MAX_VOLUME).ok(), Some(VolumePlan::Volume(40.0)));
        assert!(volume_plan(40.5).is_err(), "above the limit");
        assert!(volume_plan(-1.0).is_err());
        assert!(volume_plan(f64::NAN).is_err());
    }

    #[test]
    fn mv_command_sets_whole_and_half_steps_up_to_the_limit() {
        assert_eq!(mv_command(35.5).as_deref(), Some("MV355"));
        assert_eq!(mv_command(5.5).as_deref(), Some("MV055"));
        assert_eq!(mv_command(35.0).as_deref(), Some("MV35"));
        assert_eq!(mv_command(0.0).as_deref(), Some("MV00"));
        assert_eq!(mv_command(0.5).as_deref(), Some("MV005"));
        assert_eq!(mv_command(MAX_VOLUME).as_deref(), Some("MV40"));
        assert_eq!(mv_command(40.5), None, "above the limit");
        assert_eq!(mv_command(-0.5), None);
        assert_eq!(mv_command(35.25), None, "not a half step");
        assert_eq!(mv_command(f64::NAN), None);
    }

    #[test]
    fn the_dirac_filter_is_skipped_only_after_a_clean_sweep_that_shows_it() {
        let start = json!("Filter 1");
        let other = json!("Off");
        assert!(!dirac_back_needed(&start, Some(&start), Sweep::Clean));
        assert!(dirac_back_needed(&start, Some(&start), Sweep::Failed));
        assert!(dirac_back_needed(&start, Some(&other), Sweep::Clean));
        assert!(dirac_back_needed(&start, None, Sweep::Clean));
        // The start filter is not known.
        assert!(!dirac_back_needed(&Value::Null, None, Sweep::Failed));
        assert!(!dirac_back_needed(
            &json!("--"),
            Some(&other),
            Sweep::Failed
        ));
    }

    #[test]
    fn a_saved_address_matches_the_name_or_its_ips() {
        let ips: [IpAddr; 1] = ["192.0.2.6".parse().unwrap()];
        assert!(is_address(&json!("avr.local"), "avr.local", &ips));
        assert!(is_address(&json!("192.0.2.6"), "avr.local", &ips));
        assert!(!is_address(&json!("192.0.2.1"), "avr.local", &ips));
        assert!(is_address(&json!("192.0.2.6"), "192.0.2.6", &[]));
        assert!(!is_address(&json!("192.0.2.1"), "192.0.2.6", &[]));
        assert!(!is_address(&Value::Null, "avr.local", &ips));
    }
}
