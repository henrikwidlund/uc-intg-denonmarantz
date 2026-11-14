// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Tests of the attributes that the entities show. The expected values are the ones of the Python
//! integration.

use super::{
    EntityKind, NumberKind, SelectKind, SensorKind, SwitchKind, View, attributes, definition,
};
use crate::config_store::DeviceConfig;
use denon_marantz::State;
use denon_marantz::proto::telnet::Message;
use serde_json::json;
use serde_json::{Map, Value};

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
    }
}

/// A receiver in standby.
fn state_off() -> State {
    state(&["PWSTANDBY"])
}

/// A state from telnet lines.
fn state(lines: &[&str]) -> State {
    let mut state = State::new(true);
    for line in lines {
        let message = Message::parse(line).unwrap_or_else(|| panic!("not a message: {line}"));
        state.apply_message(&message);
    }
    state
}

fn view<'a>(state: &'a State, cfg: &'a DeviceConfig) -> View<'a> {
    View {
        state,
        cfg,
        connected: true,
        artwork_url: None,
        paused: false,
        sound_mode: super::select::active_sound_mode(state),
    }
}

fn attrs(v: &View<'_>, kind: EntityKind) -> Map<String, Value> {
    let mut out = Map::new();
    attributes(v, kind, &mut out);
    out
}

fn sensor(v: &View<'_>, kind: SensorKind) -> Map<String, Value> {
    attrs(v, EntityKind::Sensor(kind))
}

fn select(v: &View<'_>, kind: SelectKind) -> Map<String, Value> {
    attrs(v, EntityKind::Select(kind))
}

/// A receiver that is on, with a volume of -35 dB and a muted output.
const ON: &[&str] = &["PWON", "MV45", "MUON"];

#[test]
fn volume_sensors_show_db_and_the_remote_scale() {
    let (cfg, state) = (cfg(), state(ON));
    let v = view(&state, &cfg);
    let db = sensor(&v, SensorKind::VolumeDb);
    assert_eq!(json!(-35.0), db["value"]);
    assert_eq!(json!("dB"), db["unit"]);
    assert_eq!(json!("ON"), db["state"]);
    assert_eq!(json!(45.0), sensor(&v, SensorKind::VolumeAbsolute)["value"]);
    let player = attrs(&v, EntityKind::MediaPlayer);
    assert_eq!(json!(45.0), player["volume"]);
    assert_eq!(json!(true), player["muted"]);
    assert_eq!(json!("on"), sensor(&v, SensorKind::Mute)["value"]);
}

#[test]
fn values_are_empty_while_the_receiver_is_off() {
    let (cfg, state) = (cfg(), state(&["PWSTANDBY", "MV45", "MUON", "SIGAME"]));
    let v = view(&state, &cfg);
    // The mute sensor is binary: it shows `off`. Most sensors show `--`.
    assert_eq!(json!("off"), sensor(&v, SensorKind::Mute)["value"]);
    assert_eq!(json!("--"), sensor(&v, SensorKind::VolumeDb)["value"]);
    assert_eq!(json!("--"), sensor(&v, SensorKind::AudioSignal)["value"]);
    // The sleep timer clears its unit.
    let sleep = sensor(&v, SensorKind::SleepTimer);
    assert_eq!(json!("--"), sleep["value"]);
    assert_eq!(json!(""), sleep["unit"]);
    // The input source keeps its value.
    assert_ne!(json!("--"), sensor(&v, SensorKind::InputSource)["value"]);
    // Selects have one empty option, but the input source keeps its list.
    let eco = select(&v, SelectKind::EcoMode);
    assert_eq!(json!(["--"]), eco["options"]);
    assert_eq!(json!("--"), eco["current_option"]);
    let player = attrs(&v, EntityKind::MediaPlayer);
    assert_eq!(json!("OFF"), player["state"]);
    assert_eq!(json!(""), player["source"]);
    assert_eq!(json!("OFF"), attrs(&v, EntityKind::Remote)["state"]);
}

#[test]
fn entities_are_unavailable_without_a_connection() {
    let (cfg, state) = (cfg(), state(ON));
    let mut v = view(&state, &cfg);
    v.connected = false;
    assert_eq!(json!("UNAVAILABLE"), sensor(&v, SensorKind::Mute)["state"]);
    assert_eq!(
        json!("UNAVAILABLE"),
        select(&v, SelectKind::Dimmer)["state"]
    );
    assert_eq!(
        json!("UNAVAILABLE"),
        attrs(&v, EntityKind::MediaPlayer)["state"]
    );
    assert_eq!(json!("UNAVAILABLE"), attrs(&v, EntityKind::Remote)["state"]);
}

#[test]
fn select_options_have_the_order_of_the_python_integration() {
    let (cfg, state) = (cfg(), state(&["PWON", "ECOAUTO", "DIM DAR", "SPPR 2"]));
    let v = view(&state, &cfg);
    let eco = select(&v, SelectKind::EcoMode);
    assert_eq!(json!(["On", "Auto", "Off"]), eco["options"]);
    assert_eq!(json!("Auto"), eco["current_option"]);
    let dimmer = select(&v, SelectKind::Dimmer);
    assert_eq!(json!(["Off", "Dark", "Dim", "Bright"]), dimmer["options"]);
    assert_eq!(json!("Dark"), dimmer["current_option"]);
    let dirac = select(&v, SelectKind::DiracFilter);
    assert_eq!(
        json!(["Slot 1", "Slot 2", "Slot 3", "Off"]),
        dirac["options"]
    );
    // The speaker preset has numbers.
    let preset = select(&v, SelectKind::SpeakerPreset);
    assert_eq!(json!([1, 2]), preset["options"]);
    assert_eq!(json!(2), preset["current_option"]);
}

#[test]
fn an_unknown_select_value_is_empty() {
    let (cfg, state) = (cfg(), state(&["PWON"]));
    let v = view(&state, &cfg);
    let eco = select(&v, SelectKind::EcoMode);
    assert_eq!(json!("--"), eco["current_option"]);
    assert_eq!(json!(["On", "Auto", "Off"]), eco["options"]);
}

#[test]
fn sleep_timer_shows_minutes_with_a_unit() {
    let cfg = cfg();
    let on = state(&["PWON", "SLP010"]);
    let sleep = sensor(&view(&on, &cfg), SensorKind::SleepTimer);
    assert_eq!(json!(10), sleep["value"]);
    assert_eq!(json!("min"), sleep["unit"]);
    let off = state(&["PWON", "SLPOFF"]);
    let sleep = sensor(&view(&off, &cfg), SensorKind::SleepTimer);
    assert_eq!(json!("Off"), sleep["value"]);
    assert_eq!(json!(""), sleep["unit"]);
}

#[test]
fn all_zone_stereo_follows_the_receiver() {
    let cfg = cfg();
    let on = state(&["PWON", "MNZST ON"]);
    assert_eq!(
        json!("ON"),
        sensor(&view(&on, &cfg), SensorKind::AllZoneStereo)["value"]
    );
    let off = state(&["PWON", "MNZST OFF"]);
    assert_eq!(
        json!("OFF"),
        sensor(&view(&off, &cfg), SensorKind::AllZoneStereo)["value"]
    );
}

#[test]
fn media_is_cleared_when_the_source_changes() {
    let cfg = cfg();
    let playing = state(&["PWON", "SINET", "NSE0Now Playing", "NSE1A Title"]);
    let player = attrs(&view(&playing, &cfg), EntityKind::MediaPlayer);
    assert_eq!(json!("A Title"), player["media_title"]);
    assert_eq!(json!("PLAYING"), player["state"]);
    let changed = state(&["PWON", "SINET", "NSE0Now Playing", "NSE1A Title", "SICD"]);
    let player = attrs(&view(&changed, &cfg), EntityKind::MediaPlayer);
    // The old title is gone. A source that plays no media shows its name, as in the Python
    // integration.
    assert_eq!(json!("CD"), player["media_title"]);
    assert_eq!(json!("ON"), player["state"]);
}

#[test]
fn the_tuner_shows_the_frequency_and_the_band() {
    let cfg = cfg();
    let state = state(&["PWON", "SITUNER", "TFAN008750"]);
    let player = attrs(&view(&state, &cfg), EntityKind::MediaPlayer);
    assert_eq!(json!("87.50 MHz"), player["media_title"]);
    assert_eq!(json!("FM"), player["media_artist"]);
    // The station of HD radio is the album, when there is no album.
    assert_eq!(json!(""), player["media_album"]);
}

#[test]
fn a_paused_player_shows_paused() {
    let cfg = cfg();
    let state = state(&["PWON", "SINET", "NSE0Now Playing", "NSE1A Title"]);
    let mut v = view(&state, &cfg);
    assert_eq!(
        json!("PLAYING"),
        attrs(&v, EntityKind::MediaPlayer)["state"]
    );
    v.paused = true;
    assert_eq!(json!("PAUSED"), attrs(&v, EntityKind::MediaPlayer)["state"]);
    // The remote and the sensors show it as on.
    assert_eq!(json!("ON"), attrs(&v, EntityKind::Remote)["state"]);
    // A receiver that is off is off.
    let off = state_off();
    let mut v = view(&off, &cfg);
    v.paused = true;
    assert_eq!(json!("OFF"), attrs(&v, EntityKind::MediaPlayer)["state"]);
}

#[test]
fn network_audio_without_text_shows_no_title() {
    let cfg = cfg();
    let state = state(&["PWON", "SINET"]);
    let player = attrs(&view(&state, &cfg), EntityKind::MediaPlayer);
    assert_eq!(json!(""), player["media_title"]);
}

#[test]
fn the_media_player_has_the_python_features() {
    let mut c = cfg();
    let features = |c: &DeviceConfig| -> Vec<String> {
        let entity = definition(c, EntityKind::MediaPlayer);
        entity.features.expect("features")
    };
    let has = |f: &[String], name: &str| f.iter().any(|x| x == name);
    let denon = features(&c);
    assert!(has(&denon, "select_sound_mode") && has(&denon, "stop"));
    assert_eq!(23, denon.len());
    c.support_sound_mode = false;
    c.is_denon = false;
    let marantz = features(&c);
    assert!(!has(&marantz, "select_sound_mode") && !has(&marantz, "stop"));
    assert_eq!(21, marantz.len());
}

#[test]
fn sensor_definitions_have_the_python_options() {
    let c = cfg();
    let options = |kind| {
        definition(&c, EntityKind::Sensor(kind))
            .options
            .expect("options")
    };
    assert_eq!(json!("dB"), options(SensorKind::VolumeDb)["custom_unit"]);
    assert_eq!(json!(1), options(SensorKind::VolumeDb)["decimals"]);
    assert_eq!(json!("ms"), options(SensorKind::AudioDelay)["custom_unit"]);
    assert_eq!(
        json!("kHz"),
        options(SensorKind::AudioSamplingRate)["custom_unit"]
    );
    let class = |kind| {
        definition(&c, EntityKind::Sensor(kind))
            .device_class
            .expect("class")
    };
    assert_eq!("binary", class(SensorKind::Mute));
    assert_eq!("custom", class(SensorKind::Dimmer));
}

#[test]
fn entity_names_are_the_ones_of_the_python_integration() {
    let c = cfg();
    let name = |kind| definition(&c, kind).name["en"].clone();
    assert_eq!("AVR", name(EntityKind::MediaPlayer));
    assert_eq!("AVR Remote", name(EntityKind::Remote));
    assert_eq!(
        "AVR Volume dB",
        name(EntityKind::Sensor(SensorKind::VolumeDb))
    );
    assert_eq!(
        "AVR Video Out Format",
        name(EntityKind::Sensor(SensorKind::VideoHdmiSignalOut))
    );
    assert_eq!(
        "AVR Reference Level",
        name(EntityKind::Select(SelectKind::ReferenceLevelOffset))
    );
}

#[test]
fn the_sources_use_the_names_of_the_receiver() {
    let cfg = cfg();
    let state = state(&[
        "PWON",
        "SIGAME",
        "SSFUNGAME Console",
        "SSFUNSAT/CBL Apple TV",
    ]);
    let v = view(&state, &cfg);
    let input = select(&v, SelectKind::InputSource);
    let options = input["options"].as_array().expect("options");
    assert!(options.contains(&json!("Console")), "{options:?}");
    assert!(options.contains(&json!("Apple TV")), "{options:?}");
    assert_eq!(json!("Console"), input["current_option"]);
    assert_eq!(
        json!("Console"),
        sensor(&v, SensorKind::InputSource)["value"]
    );
}

#[test]
fn the_dirac_filter_shows_the_names_of_the_slots() {
    let cfg = cfg();
    let named = state(&[
        "PWON",
        "OPDLSSL1NAME Cinema",
        "OPDLSSL3NAME Music",
        "PSDIRAC 3",
    ]);
    let dirac = select(&view(&named, &cfg), SelectKind::DiracFilter);
    assert_eq!(
        json!(["Cinema", "Slot 2", "Music", "Off"]),
        dirac["options"]
    );
    assert_eq!(json!("Music"), dirac["current_option"]);
    // Only slots with a filter are options. A slot without a filter is empty (NSA).
    let empty = state(&[
        "PWON",
        "OPDLSSL1IMP ON",
        "OPDLSSL1TYP BC",
        "OPDLSSL2NAME Dirac ART",
        "OPDLSSL2TYP ART",
        "OPDLSSL3TYP NSA",
    ]);
    let dirac = select(&view(&empty, &cfg), SelectKind::DiracFilter);
    assert_eq!(json!(["Slot 1", "Dirac ART", "Off"]), dirac["options"]);
    let none = state(&[
        "PWON",
        "OPDLSSL1TYP NSA",
        "OPDLSSL2TYP NSA",
        "OPDLSSL3TYP NSA",
    ]);
    let dirac = select(&view(&none, &cfg), SelectKind::DiracFilter);
    assert_eq!(json!(["Off"]), dirac["options"]);
    // The same name in two slots gets the slot number.
    let same = state(&["PWON", "OPDLSSL1NAME Cinema", "OPDLSSL2NAME Cinema"]);
    let dirac = select(&view(&same, &cfg), SelectKind::DiracFilter);
    assert_eq!(
        json!(["Cinema (Slot 1)", "Cinema (Slot 2)", "Slot 3", "Off"]),
        dirac["options"]
    );
}

#[test]
fn the_sound_mode_list_has_the_groups_without_a_receiver_list() {
    let cfg = cfg();
    let state = state(&["PWON"]);
    let options = select(&view(&state, &cfg), SelectKind::SoundMode)["options"].clone();
    let options = options.as_array().expect("options");
    assert!(options.contains(&json!("MOVIE")), "{options:?}");
    assert!(options.contains(&json!("STEREO")), "{options:?}");
    assert!(!options.contains(&json!("MULTI CH IN")), "{options:?}");
}

/// The sound mode list of a receiver with a multichannel input.
const SOUND_MODE_LIST: [&str; 6] = [
    "OPSMLALL MOV001Stereo",
    "OPSMLALL MOV002Multi Ch In",
    "OPSMLALL MOV003Multi Ch Stereo",
    "OPSMLALL MOV004Direct",
    "OPSMLALL MOV005Pure Direct",
    "OPSMLALL END",
];

#[test]
fn the_sound_mode_select_marks_the_list_entry_of_the_reported_mode() {
    let cfg = cfg();
    for (reported, shown) in [
        ("MSSTEREO", "Stereo"),
        ("MSMULTI CH IN", "Multi Ch In"),
        ("MSMCH STEREO", "Multi Ch Stereo"),
        ("MSDIRECT", "Direct"),
        ("MSPURE DIRECT", "Pure Direct"),
    ] {
        let mut lines = vec!["PWON"];
        lines.extend(SOUND_MODE_LIST);
        lines.push(reported);
        let state = state(&lines);
        let v = view(&state, &cfg);
        assert_eq!(
            json!(shown),
            select(&v, SelectKind::SoundMode)["current_option"],
            "{reported}"
        );
        assert_eq!(
            json!(shown),
            attrs(&v, EntityKind::MediaPlayer)["sound_mode"]
        );
    }
    // A mode that is not in the list has no option.
    let mut lines = vec!["PWON"];
    lines.extend(SOUND_MODE_LIST);
    lines.push("MSDOLBY ATMOS");
    assert_eq!(None, super::select::active_sound_mode(&state(&lines)));
}

#[test]
fn without_a_receiver_list_the_sound_mode_select_marks_the_group() {
    let cfg = cfg();
    for (reported, shown) in [
        ("MSSTEREO", "STEREO"),
        ("MSDOLBY ATMOS", "DOLBY DIGITAL"),
        // As in the Python integration: Multi Ch In is part of the movie group.
        ("MSMULTI CH IN", "MOVIE"),
    ] {
        let state = state(&["PWON", reported]);
        assert_eq!(
            json!(shown),
            select(&view(&state, &cfg), SelectKind::SoundMode)["current_option"],
            "{reported}"
        );
    }
}

#[test]
fn the_extra_selects_show_the_receiver_values() {
    let cfg = cfg();
    let state = state(&[
        "PWON",
        "PSMULTEQ:AUDYSSEY",
        "BTTX SP",
        "TFAN008750",
        "MSQUICK3",
    ]);
    let v = view(&state, &cfg);
    let multi = select(&v, SelectKind::MultiEq);
    assert_eq!(
        json!(["Off", "Flat", "L/R Bypass", "Reference"]),
        multi["options"]
    );
    assert_eq!(json!("Reference"), multi["current_option"]);
    let band = select(&v, SelectKind::TunerBand);
    assert_eq!(json!(["AM", "FM"]), band["options"]);
    let output = select(&v, SelectKind::BluetoothOutput);
    assert_eq!(json!("Bluetooth + Speakers"), output["current_option"]);
    let quick = select(&v, SelectKind::QuickSelect);
    assert_eq!(5, quick["options"].as_array().map_or(0, Vec::len));
    assert_eq!(json!("Quick Select 3"), quick["current_option"]);
}

#[test]
fn the_switches_show_on_off_and_unknown() {
    let cfg = cfg();
    let known = state(&["PWON", "PSDYNEQ ON", "PSTONE CTRL OFF"]);
    let v = view(&known, &cfg);
    let switch = |kind| attrs(&v, EntityKind::Switch(kind))["state"].clone();
    assert_eq!(json!("ON"), switch(SwitchKind::DynamicEq));
    assert_eq!(json!("OFF"), switch(SwitchKind::ToneControl));
    assert_eq!(json!("UNKNOWN"), switch(SwitchKind::Trigger1));
    let mut gone = view(&known, &cfg);
    gone.connected = false;
    assert_eq!(
        json!("UNAVAILABLE"),
        attrs(&gone, EntityKind::Switch(SwitchKind::DynamicEq))["state"]
    );
}

#[test]
fn the_tone_numbers_show_db() {
    let cfg = cfg();
    let state = state(&["PWON", "PSBAS 52"]);
    let v = view(&state, &cfg);
    let bass = attrs(&v, EntityKind::Number(NumberKind::Bass));
    assert_eq!(json!(2), bass["value"]);
    let treble = attrs(&v, EntityKind::Number(NumberKind::Treble));
    assert_eq!(Value::Null, treble["value"]);
}

#[test]
fn the_dirac_sensor_shows_the_active_filter() {
    let cfg = cfg();
    let state = state(&[
        "PWON",
        "OPDLSSL2NAME Dirac ART",
        "OPDLSSL2TYP ART",
        "PSDIRAC 2",
    ]);
    let v = view(&state, &cfg);
    assert_eq!(
        json!("Dirac ART"),
        sensor(&v, SensorKind::DiracFilter)["value"]
    );
}

#[test]
fn volume_and_delay_are_empty_while_they_are_unknown() {
    let cfg = cfg();
    let state = state(&["PWON"]);
    let v = view(&state, &cfg);
    assert_eq!(json!("--"), sensor(&v, SensorKind::VolumeDb)["value"]);
    assert_eq!(json!("--"), sensor(&v, SensorKind::VolumeAbsolute)["value"]);
    assert_eq!(json!("--"), sensor(&v, SensorKind::AudioDelay)["value"]);
}
