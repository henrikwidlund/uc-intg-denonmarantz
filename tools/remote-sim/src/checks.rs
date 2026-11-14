// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The checks. They follow the steps of `tools/manual_test.sh`.

use crate::oracle::{self, Oracle};
use crate::ws::{field_ids, field_value};
use crate::{Check, Ctx, Fail};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio::time::sleep;

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

/// Values that come from the web pages of the receiver.
const WEB_PAGE_VALUES: [&str; 6] = [
    "select.monitor_output",
    "sensor.monitor_output",
    "select.picture_mode",
    "sensor.picture_mode",
    "select.digital_codec",
    "sensor.digital_codec",
];

/// Values of selects and sensors that differ from the values of the receiver.
fn mismatches(ctx: &Ctx) -> Vec<String> {
    let Some(oracle) = &ctx.oracle else {
        return Vec::new();
    };
    let state = oracle.state();
    let on = matches!(
        state.zone(denon_marantz::Zone::Main).power,
        Some(denon_marantz::proto::Power::On)
    );
    if !on || !is_on(&ctx.client.text(&ctx.mp(), "state")) {
        return Vec::new();
    }
    let mut bad = Vec::new();
    for (name, expected) in oracle::expected(&state) {
        let Some((kind, rest)) = name.split_once('.') else {
            continue;
        };
        let id = format!("{kind}.{rest}.{}", ctx.key);
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
    loop {
        let bad = mismatches(ctx);
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

/// Switch the receiver on. A power on can set the power-on volume of the receiver: it is lowered
/// at once if it is above the limit.
async fn power_on(ctx: &mut Ctx) -> Result<String, Fail> {
    let mp = ctx.mp();
    ctx.client
        .command_ok("media_player", &mp, "on", json!({}))
        .await?;
    ctx.client
        .wait_attr(&mp, "state", 20, |v| v.as_str().is_some_and(is_on))
        .await?;
    ctx.client.listen(1).await?;
    if let Some(v) = current_volume(ctx).filter(|v| *v > MAX_VOLUME) {
        ctx.client
            .command_ok(
                "media_player",
                &mp,
                "volume",
                json!({ "volume": MAX_VOLUME }),
            )
            .await?;
        return Err(Fail::new(format!(
            "the volume after power on was {v} (limit {MAX_VOLUME}), it was lowered"
        )));
    }
    Ok(String::new())
}

async fn ensure_on(ctx: &mut Ctx) -> Result<(), Fail> {
    let state = ctx.client.text(&ctx.mp(), "state");
    if !is_on(&state) {
        power_on(ctx).await?;
    }
    Ok(())
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
    ctx.client.answer(json!({ "choice": address })).await?;
    ctx.client.setup_done(20).await?;
    Ok(format!("Dirac in the setup: {dirac}"))
}

pub async fn entities(ctx: &mut Ctx) -> Check {
    let list = entity_list(ctx).await?;
    let mp = list
        .iter()
        .filter_map(|e| e["entity_id"].as_str())
        .find(|id| id.starts_with("media_player."))
        .ok_or_else(|| Fail::new("no media player entity"))?;
    ctx.key = mp.trim_start_matches("media_player.").to_owned();
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
    Ok(format!(
        "serial {}, {selects} selects, {sensors} sensors",
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
    ensure!(simple > 300, "{simple} simple commands");
    ensure!(pages == 15, "{pages} UI pages");
    ensure!(options["button_mapping"].is_array(), "no button mapping");
    let features = strings(&remote["features"]);
    ensure!(
        features.contains(&"send_cmd".to_owned()),
        "features: {features:?}"
    );
    Ok(format!("{simple} simple commands, {pages} pages"))
}

// ---------------------------------------------------------------- wake and sleep

async fn connects(ctx: &Ctx) -> usize {
    ctx.driver.log_text().await.matches("connected in").count()
}

pub async fn standby(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let before = connects(ctx).await;
    ctx.client.send_event("enter_standby").await?;
    ctx.client.listen(3).await?;
    let state = ctx.client.text(&mp, "state");
    ensure!(state != "UNAVAILABLE", "state in standby: {state}");
    ctx.client.send_event("exit_standby").await?;
    let start = Instant::now();
    while connects(ctx).await == before {
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

// ---------------------------------------------------------------- control

pub async fn selects(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mut note = Vec::new();
    for kind in ["dimmer", "eco_mode"] {
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

pub async fn remote_commands(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let remote = ctx.remote();
    let dimmer = ctx.select("dimmer");
    let sensor = ctx.sensor("dimmer");
    let old = ctx.client.text(&dimmer, "current_option");
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

pub async fn mute(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let muted = ctx.client.attr(&mp, "muted") == Some(&json!(true));
    for target in [!muted, muted] {
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

pub async fn source(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let old = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    // The receiver lists a `Source` entry that cannot be selected: it is not used here.
    let Some(new) = list
        .iter()
        .find(|s| **s != old && *s != "Source" && ctx.opt.may_select(s))
        .cloned()
    else {
        return Err(Fail::skip("only one source"));
    };
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
    let tries = restore_source(ctx, &old).await?;
    verify_all(ctx, 6).await?;
    let odd = if list.iter().any(|s| s == "Source") {
        ", the list has a `Source` entry"
    } else {
        ""
    };
    Ok(format!(
        "{old} -> {new} in {took} ms, back in {tries} tries{odd}"
    ))
}

/// Select the old source again. The receiver can ignore a source command that comes soon after
/// another one, so the command is sent again after 4 s.
async fn restore_source(ctx: &mut Ctx, old: &str) -> Result<u32, Fail> {
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
        if ctx.client.wait_text(&mp, "source", 4, old).await.is_ok() {
            return Ok(tries);
        }
    }
    Err(Fail::new(format!(
        "the source did not go back to {old} in 3 tries"
    )))
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
    let new = pick_sound_mode(&list, &old).ok_or_else(|| Fail::skip("no other sound mode"))?;
    select_sound(ctx, &new).await?;
    ctx.client
        .wait_attr(&mp, "sound_mode", 10, |v| {
            v.as_str().is_some_and(|m| m != old)
        })
        .await?;
    let shown = ctx.client.text(&mp, "sound_mode");
    ensure!(
        ctx.client.text(&ctx.sensor("sound_mode"), "value") != "--",
        "the sound mode sensor is empty"
    );
    verify_all(ctx, 6).await?;
    let restored = restore_sound(ctx, &old, &list).await;
    verify_all(ctx, 6).await?;
    Ok(format!("{old} -> {new} (receiver: {shown}), {restored}"))
}

/// Select the old mode again. The receiver can report a name that is not in the list.
async fn restore_sound(ctx: &mut Ctx, old: &str, list: &[String]) -> String {
    let mp = ctx.mp();
    let Some(name) = list.iter().find(|m| m.eq_ignore_ascii_case(old)) else {
        return format!("the old mode {old} is not in the list, not restored");
    };
    if select_sound(ctx, name).await.is_err() {
        return "restore failed".into();
    }
    match ctx.client.wait_text(&mp, "sound_mode", 10, old).await {
        Ok(()) => "restored".into(),
        Err(e) => format!("not restored: {e}"),
    }
}

pub async fn volume(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let start = current_volume(ctx).ok_or_else(|| Fail::new("no volume"))?;
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
    let step = ctx
        .client
        .attr(&mp, "volume")
        .and_then(Value::as_f64)
        .unwrap_or(start);
    ensure!(step == start, "unstable volume");
    // Down first: the volume never goes above the start value.
    ctx.client
        .command_ok(
            "media_player",
            &mp,
            "volume",
            json!({ "volume": start - 2.0 }),
        )
        .await?;
    ctx.client
        .wait_attr(&mp, "volume", 6, |v| v.as_f64() == Some(start - 2.0))
        .await?;
    let sensor = ctx.sensor("volume_db");
    ctx.client
        .wait_attr(&sensor, "value", 6, |v| {
            v.as_f64() == Some(start - 2.0 - 80.0)
        })
        .await?;
    ctx.client
        .command_ok("media_player", &mp, "volume_up", json!({}))
        .await?;
    let up = ctx
        .client
        .wait_attr(&mp, "volume", 6, |v| {
            v.as_f64().is_some_and(|x| x > start - 2.0)
        })
        .await?;
    let up = up.as_f64().unwrap_or(start);
    ensure!(
        up <= start,
        "volume_up went to {up}, above the start value {start}"
    );
    ctx.client
        .command_ok("media_player", &mp, "volume", json!({ "volume": start }))
        .await?;
    ctx.client
        .wait_attr(&mp, "volume", 6, |v| v.as_f64() == Some(start))
        .await?;
    Ok(format!("volume {start}, up step gave {up}, restored"))
}

pub async fn power(ctx: &mut Ctx) -> Check {
    if ctx.opt.no_power {
        return Err(Fail::skip("--no-power"));
    }
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    // Mute first: a power on can start with another volume.
    let muted = ctx.client.attr(&mp, "muted") == Some(&json!(true));
    if !muted {
        ctx.client
            .command_ok("media_player", &mp, "mute_toggle", json!({}))
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
    if !muted {
        ctx.client
            .command_ok("media_player", &mp, "mute_toggle", json!({}))
            .await?;
        ctx.client
            .wait_attr(&mp, "muted", 6, |v| *v == json!(false))
            .await?;
    }
    verify_fresh(ctx).await?;
    Ok(format!("off, then on after {took} ms, values are back"))
}

// ---------------------------------------------------------------- setup changes

/// Change a setup option of the receiver and wait until the setup is done.
async fn configure(ctx: &mut Ctx, address: &str, telnet: Option<bool>) -> Result<(), Fail> {
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
    ensure!(
        field_ids(&page).contains(&"connection_mode".to_owned()),
        "configure page: {page}"
    );
    let mut values = json!({ "address": address });
    if let Some(telnet) = telnet {
        values["connection_mode"] = if telnet { "use_telnet" } else { "use_http" }.into();
    }
    ctx.client.answer(values).await?;
    ctx.client.setup_done(20).await
}

/// A Remote that reads the entities again after a mode change.
async fn refresh_remote(ctx: &mut Ctx) -> Result<usize, Fail> {
    ctx.reconnect().await?;
    ctx.client.forget_states();
    subscribe_all(ctx).await
}

pub async fn http_mode(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let old_sound = ctx.client.text(&ctx.mp(), "sound_mode");
    let result = http_checks(ctx).await;
    // Always go back to telnet, also after a failure.
    let address = ctx.opt.address.clone();
    let back = async {
        configure(ctx, &address, Some(true)).await?;
        refresh_remote(ctx).await?;
        wait_available(ctx, 20).await
    }
    .await;
    match (result, back) {
        (Ok(note), Ok(_)) => {
            let mp = ctx.mp();
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
            let wait = if old_sound.is_empty() { 0 } else { 10 };
            let list = ctx
                .client
                .wait_attr(&mp, "sound_mode_list", wait, has_old)
                .await
                .ok()
                .or_else(|| ctx.client.attr(&mp, "sound_mode_list").cloned())
                .unwrap_or(Value::Null);
            let list = strings(&list);
            let restored = restore_sound(ctx, &old_sound, &list).await;
            Ok(format!("{note}, {restored}"))
        }
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(Fail::new(format!("back to telnet: {e}"))),
    }
}

async fn http_checks(ctx: &mut Ctx) -> Check {
    let mp = ctx.mp();
    let before = entity_ids(ctx).await?.len();
    let address = ctx.opt.address.clone();
    configure(ctx, &address, Some(false)).await?;
    let count = refresh_remote(ctx).await?;
    ensure!(
        count < before,
        "{count} entities in HTTP mode, {before} with telnet"
    );
    let list = entity_ids(ctx).await?;
    let count = |prefix: &str| list.iter().filter(|i| i.starts_with(prefix)).count();
    let found = (
        count("select."),
        count("sensor."),
        count("switch."),
        count("number."),
    );
    let config = read_config(ctx).await?;
    let expected = http_counts(config_device(ctx, &config)?);
    ensure!(
        found == expected,
        "selects, sensors, switches, numbers: {found:?}, expected {expected:?}"
    );
    wait_available(ctx, 20).await?;
    let (selects, sensors, switches, numbers) = found;
    let mut note = vec![format!(
        "{selects} selects, {sensors} sensors, {switches} switches, {numbers} numbers"
    )];

    let start = Instant::now();
    let (old, new) = change_select(ctx, "dimmer", 12).await?;
    note.push(format!("dimmer {} ms", start.elapsed().as_millis()));
    ctx.client
        .wait_text(&ctx.sensor("dimmer"), "value", 12, &new)
        .await?;
    set_option(ctx, "dimmer", &old, 12).await?;

    let old = ctx.client.text(&mp, "source");
    let list = strings(ctx.client.attr(&mp, "source_list").unwrap_or(&Value::Null));
    ensure!(!list.is_empty(), "no source list over HTTP");
    if let Some(new) = list
        .iter()
        .find(|s| **s != old && *s != "Source" && ctx.opt.may_select(s))
        .cloned()
    {
        let start = Instant::now();
        ctx.client
            .command_ok(
                "media_player",
                &mp,
                "select_source",
                json!({ "source": new }),
            )
            .await?;
        ctx.client.wait_text(&mp, "source", 12, &new).await?;
        note.push(format!("source {} ms", start.elapsed().as_millis()));
        ctx.client
            .command_ok(
                "media_player",
                &mp,
                "select_source",
                json!({ "source": old }),
            )
            .await?;
        ctx.client.wait_text(&mp, "source", 12, &old).await?;
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
    let start = Instant::now();
    select_sound(ctx, group).await?;
    ctx.client
        .wait_attr(&mp, "sound_mode", 15, |v| {
            v.as_str().is_some_and(|m| m != old)
        })
        .await?;
    note.push(format!("sound mode {} ms", start.elapsed().as_millis()));
    note.push(format!(
        "sound mode was {old}, restored after the switch to telnet"
    ));
    Ok(note.join(", "))
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

pub async fn unreachable(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
    let mp = ctx.mp();
    let address = ctx.opt.address.clone();
    // 192.0.2.0/24 is for documentation: nothing answers.
    configure(ctx, "192.0.2.1", None).await?;
    let result = async {
        let down = ctx.client.wait_text(&mp, "state", 20, "UNAVAILABLE").await;
        if down.is_err() {
            return Ok("the driver found the receiver again by its serial number".to_owned());
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
        ensure!(
            response["code"] != 200,
            "a command worked without a connection"
        );
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

// ---------------------------------------------------------------- configuration file

async fn read_config(ctx: &Ctx) -> Result<Value, Fail> {
    let text = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    serde_json::from_str(&text).map_err(|e| Fail::new(format!("config.json: {e}")))
}

/// Fields of the file of the Python integration. `signal_poll_secs` is new: it is not written
/// while it is 0.
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

/// The selects, sensors, switches and numbers in HTTP mode for the receiver kind in
/// `config.json`. Receivers read with `AppCommand` get MultEQ, tone control, Dynamic EQ, bass and
/// treble. The 2016 generation also gets 6 signal sensors.
fn http_counts(device: &Value) -> (usize, usize, usize, usize) {
    let app_command = device["update_method"] == "app_command";
    let signal = app_command && device["receiver_type"] == "avr_x2016";
    let extra = usize::from(app_command);
    (6 + extra, 7 + 6 * usize::from(signal), 2 * extra, 2 * extra)
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
    ensure!(
        device.get("receiver_type").is_some() && device.get("update_method").is_some(),
        "receiver_type or update_method missing in config.json"
    );
    Ok(format!(
        "id is the serial number, address {}, use_telnet {}, receiver_type {}, update_method {}",
        device["address"], device["use_telnet"], device["receiver_type"], device["update_method"]
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

/// Start the driver again with a changed configuration file.
async fn restart(ctx: &mut Ctx) -> Result<(), Fail> {
    ctx.driver.start().await?;
    ctx.reconnect().await
}

pub async fn python_config(ctx: &mut Ctx) -> Check {
    let ids = entity_ids(ctx).await?;
    let good = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    let mut config = read_config(ctx).await?;
    // The file of the Python integration has no field for the signal read.
    for device in config.as_array_mut().into_iter().flatten() {
        if let Some(device) = device.as_object_mut() {
            device.remove("signal_poll_secs");
        }
    }
    ctx.driver.stop().await;
    tokio::fs::write(ctx.driver.config_file(), config.to_string()).await?;
    let result = async {
        restart(ctx).await?;
        let after = entity_ids(ctx).await?;
        ensure!(
            after == ids,
            "entities differ: {} before, {} after",
            ids.len(),
            after.len()
        );
        subscribe_all(ctx).await?;
        let state = wait_available(ctx, 20).await?;
        Ok(format!("same {} entities, state {state}", after.len()))
    }
    .await;
    ctx.driver.stop().await;
    tokio::fs::write(ctx.driver.config_file(), good).await?;
    restart(ctx).await?;
    subscribe_all(ctx).await?;
    settle(ctx).await?;
    result
}

pub async fn broken_config(ctx: &mut Ctx) -> Check {
    let good = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    ctx.driver.stop().await;
    tokio::fs::write(ctx.driver.config_file(), "[{\"broken\": ").await?;
    let result = async {
        restart(ctx).await?;
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
    tokio::fs::write(ctx.driver.config_file(), good).await?;
    restart(ctx).await?;
    subscribe_all(ctx).await?;
    wait_available(ctx, 20).await?;
    settle(ctx).await?;
    result
}

// ---------------------------------------------------------------- end

pub async fn restore_power(ctx: &mut Ctx) -> Check {
    if ctx.was_on {
        return Ok(String::new());
    }
    let mp = ctx.mp();
    ctx.client
        .command_ok("media_player", &mp, "off", json!({}))
        .await?;
    ctx.client.wait_text(&mp, "state", 15, "OFF").await?;
    sleep(Duration::from_millis(200)).await;
    Ok("the receiver is off again, like at the start".into())
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
    // The unreachable and broken file checks cause some warnings.
    ensure!(errors.is_empty(), "{} ERROR lines in the log", errors.len());
    Ok(format!(
        "{} warnings, no errors. Log: {}",
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
    ensure_on(ctx).await?;
    let mut problems = Vec::new();
    // The receiver ignores the Audyssey settings while a Dirac filter is selected.
    let dirac = ctx.select("dirac_filter");
    let dirac_original = ctx
        .client
        .attr(&dirac, "current_option")
        .cloned()
        .unwrap_or(Value::Null);
    if dirac_original != json!("Off") && dirac_original != Value::Null {
        sweep_one(ctx, "dirac_filter", &json!("Off")).await?;
    }
    // The reference level needs Dynamic EQ.
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
    let mut summary = Vec::new();
    let mut verified = 0;
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
        if let Some(tally) = sweep_select(ctx, kind, &mut problems).await? {
            verified += tally.verified;
            summary.push(format!("{kind}: {}", tally.describe()));
        }
    }
    if dirac_original != Value::Null
        && dirac_original != json!("--")
        && let Err(e) = sweep_one(ctx, "dirac_filter", &dirac_original).await
    {
        problems.push(format!("dirac_filter back to {dirac_original}: {e}"));
    }
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

/// Set one option, wait for the select and its sensor, then compare with the receiver.
async fn sweep_one(ctx: &mut Ctx, kind: &str, option: &Value) -> Result<Outcome, Fail> {
    let select = ctx.select(kind);
    let log_start = ctx.driver.log_text().await.len();
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
        && let Some(reason) = ctx
            .driver
            .log_text()
            .await
            .get(log_start..)
            .and_then(|new| new.split_once(IGNORED))
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
/// reset them.
fn skipped(name: &str) -> bool {
    [
        "ZONE_FAVORITE",
        "QUICK_",
        "SMART_",
        "RESET",
        "MEMORY",
        "VOLUME",
        "NETWORK_RESTART",
    ]
    .iter()
    .any(|p| name.contains(p))
}

/// Send every simple command of the remote entity, and check the entities against the receiver.
pub async fn simple_commands(ctx: &mut Ctx) -> Check {
    ensure_on(ctx).await?;
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
    let names: Vec<&str> = table
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?;
            (parts.nth(1)? != "marantz").then_some(name)
        })
        .filter(|n| offered.iter().any(|o| o == n) && !skipped(n))
        .collect();
    ensure!(names.len() > 250, "only {} commands to send", names.len());
    let mut problems = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mp = ctx.mp();
    for (i, name) in names.iter().enumerate() {
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
        if let Some(v) = current_volume(ctx).filter(|v| *v > MAX_VOLUME) {
            ctx.client
                .command_ok(
                    "media_player",
                    &mp,
                    "volume",
                    json!({ "volume": MAX_VOLUME }),
                )
                .await?;
            problems.push(format!("{name}: the volume went to {v}, it was lowered"));
        }
        if i % 10 == 9 {
            if let Err(e) = verify_all(ctx, 6).await {
                problems.push(format!("after {name}: {e}"));
            }
            if let Err(e) = ensure_on(ctx).await {
                return Err(Fail::new(format!(
                    "after {name}: {e}. Earlier problems: {}",
                    problems.join(" | ")
                )));
            }
        }
    }
    verify_fresh(ctx)
        .await
        .map_err(|e| Fail::new(format!("at the end: {e}")))?;
    if problems.is_empty() {
        // The effect of one command is not checked: only that the entities showed the values of
        // the receiver after each group of 10 commands.
        Ok(format!(
            "{} commands accepted ({} refused with a reason: {refused:?}), the effect of each is not verified, entities matched the receiver after every 10 commands",
            names.len() - refused.len(),
            refused.len()
        ))
    } else {
        problems.truncate(12);
        Err(Fail::new(format!(
            "{} commands sent. Problems: {}",
            names.len(),
            problems.join(" | ")
        )))
    }
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
    let (response, _) = ctx
        .client
        .command("switch", &tone, "toggle", json!({}))
        .await?;
    if [409, 503].contains(&response["code"].as_u64().unwrap_or(0)) {
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

    let bass = format!("number.bass.{}", ctx.key);
    let start = ctx.client.attr(&bass, "value").and_then(Value::as_i64);
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
