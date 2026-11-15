// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Allocations of the entity update path. Every burst of events of the receiver builds the
//! attributes of all entities. Run it alone, because the counter is global:
//! `cargo test --locked alloc_tests -- --ignored --nocapture`.

use super::{EntityKind, SensorKind, View, attributes};
use crate::config_store::DeviceConfig;
use denon_marantz::State;
use denon_marantz::proto::telnet::Message;
use serde_json::Map;

fn cfg() -> DeviceConfig {
    DeviceConfig {
        id: "SERIAL0000001".into(),
        name: "AVR".into(),
        address: "192.0.2.1".into(),
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
        signal_poll_secs: 0,
        receiver_type: None,
        update_method: None,
        hdmi_output_over_http: None,
        quick_select_count: None,
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

/// The attributes of the entities that `keep` takes, as `emit` builds them.
fn build(state: &State, cfg: &DeviceConfig) -> usize {
    build_some(state, cfg, |_| true)
}

fn build_some(state: &State, cfg: &DeviceConfig, keep: impl Fn(EntityKind) -> bool) -> usize {
    let view = View {
        state,
        cfg,
        connected: true,
        artwork_url: None,
        paused: false,
        sound_mode: super::select::active_sound_mode(state),
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
#[ignore = "measures allocations: run it alone"]
fn the_update_path_allocations() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let (state, cfg) = (state(), cfg());
    // Warm up the tables that are built once.
    build(&state, &cfg);
    let before = dhat::HeapStats::get();
    let runs = 100;
    let mut keys = 0;
    for _ in 0..runs {
        keys += build(&state, &cfg);
    }
    let after = dhat::HeapStats::get();
    let blocks = after.total_blocks - before.total_blocks;
    let bytes = after.total_bytes - before.total_bytes;
    println!(
        "entities {}, attributes {}, per update: {} allocations, {} bytes",
        EntityKind::all(&cfg).count(),
        keys / runs,
        blocks / u64::try_from(runs).unwrap_or(1),
        bytes / u64::try_from(runs).unwrap_or(1)
    );
}

#[test]
#[ignore = "measures allocations: run it alone"]
fn the_state_copy_allocations() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let state = state();
    let before = dhat::HeapStats::get();
    let runs = 100_u64;
    for _ in 0..runs {
        std::hint::black_box(state.clone());
    }
    let after = dhat::HeapStats::get();
    println!(
        "a copy of the state: {} allocations, {} bytes",
        (after.total_blocks - before.total_blocks) / runs,
        (after.total_bytes - before.total_bytes) / runs
    );
}

#[test]
#[ignore = "measures allocations: run it alone"]
fn the_volume_update_allocations() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let (state, cfg) = (state(), cfg());
    let keep = |kind| {
        matches!(
            kind,
            EntityKind::MediaPlayer
                | EntityKind::Sensor(SensorKind::VolumeDb | SensorKind::VolumeAbsolute)
        )
    };
    build_some(&state, &cfg, keep);
    let before = dhat::HeapStats::get();
    let runs = 100_u64;
    for _ in 0..runs {
        build_some(&state, &cfg, keep);
    }
    let after = dhat::HeapStats::get();
    println!(
        "per volume update: {} allocations, {} bytes",
        (after.total_blocks - before.total_blocks) / runs,
        (after.total_bytes - before.total_bytes) / runs
    );
}
