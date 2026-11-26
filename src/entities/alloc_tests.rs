// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Allocations of the entity update path. Every burst of events of the receiver builds the
//! attributes of all entities. dhat counts the allocations of all threads, so run these tests
//! alone: `cargo test --locked --features dhat-heap dhat_ -- --ignored --test-threads=1`.
//! Add `--nocapture` to see the measured values.

use super::{EntityKind, SensorKind, View, attributes};
use crate::config_store::DeviceConfig;
use denon_marantz::State;
use denon_marantz::proto::telnet::Message;
use serde_json::Map;
use std::sync::{Mutex, PoisonError};

/// Runs of the measured code in each test.
const RUNS: u64 = 100;

/// Only one dhat profiler can run at a time.
static LOCK: Mutex<()> = Mutex::new(());

/// Runs `warm_up` once, then `f` [`RUNS`] times. Returns the mean allocations and bytes of one run
/// of `f`, rounded up.
fn measure(warm_up: impl FnOnce(), mut f: impl FnMut()) -> (u64, u64) {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let _profiler = dhat::Profiler::builder().testing().build();
    warm_up();
    let before = dhat::HeapStats::get();
    for _ in 0..RUNS {
        f();
    }
    let after = dhat::HeapStats::get();
    (
        (after.total_blocks - before.total_blocks).div_ceil(RUNS),
        (after.total_bytes - before.total_bytes).div_ceil(RUNS),
    )
}

fn cfg() -> DeviceConfig {
    DeviceConfig {
        id: "SERIAL0000001".into(),
        id_kind: crate::config_store::IdKind::Serial,
        serial: None,
        mac: None,
        name: "AVR".into(),
        address: "192.0.2.1".into(),
        host: None,
        support_sound_mode: true,
        show_all_inputs: false,
        use_telnet: true,
        update_audyssey: false,
        zone2: false,
        zone3: false,
        volume_step: 0.5,
        timeout: 2000,
        is_denon: true,
        is_dirac_supported: Some(true),
        signal_poll_secs: Some(0),
        receiver_type: None,
        update_method: None,
        comm_api_version: None,
        has_tuner: None,
        tuner_presets_lettered: None,
        tuner: None,
        tuner_preset_names: Vec::new(),
        network_favorite_names: Vec::new(),
        info: None,
        missing: denon_marantz::MissingSources::default(),
    }
}

fn state() -> State {
    let mut state = State::new(true);
    for line in [
        "PWON",
        "MV45",
        "MUOFF",
        "SINET",
        "MSSTEREO",
        "NSE0Now Playing",
        "NSE1A Title",
        "NSE2An Artist",
        "SSFUNGAME Console",
        "DIM DAR",
        "ECOAUTO",
        "PSDYNEQ ON",
        "OPDLSSL1NAME Cinema",
        "OPDLSSL1TYP BC",
    ] {
        let message = Message::parse(line).expect("message");
        state.apply_message(&message);
    }
    state
}

/// The attributes of all entities. Returns the number of attributes.
fn build(state: &State, cfg: &DeviceConfig) -> usize {
    build_some(state, cfg, |_| true)
}

/// The attributes of the entities that `keep` takes, as `emit` builds them. Returns the number of
/// attributes.
fn build_some(state: &State, cfg: &DeviceConfig, keep: impl Fn(EntityKind) -> bool) -> usize {
    let view = View {
        state,
        cfg,
        connected: true,
        artwork_url: None,
        paused: false,
        sound_mode: super::select::active_sound_mode(state, true),
        sound_groups: true,
        heos: None,
        position: None,
    };
    let mut keys = 0;
    for kind in EntityKind::all(cfg).filter(|k| keep(*k)) {
        let mut attrs = Map::new();
        attributes(&view, kind, &mut attrs);
        keys += attrs.len();
    }
    keys
}

#[test]
#[ignore = "counts the allocations of all threads: run it alone"]
fn dhat_update_path() {
    let (state, cfg) = (state(), cfg());
    let mut keys = 0;
    // The warm-up builds the tables that are built once.
    let (blocks, bytes) = measure(
        || {
            keys = build(&state, &cfg);
        },
        || {
            std::hint::black_box(build(&state, &cfg));
        },
    );
    println!(
        "entities {}, attributes {keys}, per update: {blocks} allocations, {bytes} bytes",
        EntityKind::all(&cfg).count(),
    );
    assert!(blocks <= 750, "{blocks} allocations");
}

#[test]
#[ignore = "counts the allocations of all threads: run it alone"]
fn dhat_state_copy() {
    let state = state();
    let (blocks, bytes) = measure(
        || {},
        || {
            std::hint::black_box(state.clone());
        },
    );
    println!("a copy of the state: {blocks} allocations, {bytes} bytes");
    assert!(blocks <= 10, "{blocks} allocations");
}

#[test]
#[ignore = "counts the allocations of all threads: run it alone"]
fn dhat_volume_update() {
    let (state, cfg) = (state(), cfg());
    let keep = |kind| {
        matches!(
            kind,
            EntityKind::MediaPlayer
                | EntityKind::Sensor(SensorKind::VolumeDb | SensorKind::VolumeAbsolute)
        )
    };
    let (blocks, bytes) = measure(
        || {
            build_some(&state, &cfg, keep);
        },
        || {
            std::hint::black_box(build_some(&state, &cfg, keep));
        },
    );
    println!("per volume update: {blocks} allocations, {bytes} bytes");
    assert!(blocks <= 64, "{blocks} allocations");
}
