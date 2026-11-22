// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Entity commands against a fake receiver: the commands that the library sends.

use super::commands::{Expected, run};
use crate::config_store::DeviceConfig;
use crate::entities::{EntityKind, NumberKind, SelectKind, SwitchKind};
use crate::errors::ServiceError;
use crate::test_support::Fake;
use serde_json::{Map, Value, json};

fn cfg() -> DeviceConfig {
    DeviceConfig {
        id: "SERIAL0000001".into(),
        id_kind: crate::config_store::IdKind::Serial,
        serial: None,
        mac: None,
        name: "AVR".into(),
        address: "127.0.0.1".into(),
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
        comm_api_version: None,
        has_tuner: None,
        tuner_presets_lettered: None,
        tuner: None,
        tuner_preset_names: Vec::new(),
        network_favorite_names: Vec::new(),
        info: None,
    }
}

type Case<'a> = (EntityKind, &'a str, Option<&'a Map<String, Value>>);

fn params(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

/// Run a command on a receiver that is on or in standby. Returns the result and the commands that
/// the receiver got.
async fn send(
    power_on: bool,
    kind: EntityKind,
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
) -> (Result<(), ServiceError>, Vec<String>) {
    let fake = Fake::start(power_on).await;
    let avr = fake.avr().await;
    let result = run(
        &avr,
        None,
        &cfg(),
        &Expected::default(),
        kind,
        cmd_id,
        params,
    )
    .await;
    (result, fake.commands())
}

async fn sent(kind: EntityKind, cmd_id: &str, params: Option<&Map<String, Value>>) -> Vec<String> {
    let (result, commands) = send(true, kind, cmd_id, params).await;
    result.expect("the command works");
    commands
}

#[tokio::test]
async fn stop_without_heos_is_refused_on_marantz() {
    let fake = Fake::start(true).await;
    let avr = fake.avr().await;
    let marantz = DeviceConfig {
        is_denon: false,
        ..cfg()
    };
    let result = run(
        &avr,
        None,
        &marantz,
        &Expected::default(),
        EntityKind::MediaPlayer,
        "stop",
        None,
    )
    .await;
    assert!(
        matches!(result, Err(ServiceError::Blocked(_))),
        "{result:?}"
    );
    assert_eq!(Vec::<String>::new(), fake.commands());
}

#[tokio::test]
async fn the_switches_send_their_commands() {
    let switch = |kind| EntityKind::Switch(kind);
    for (kind, cmd, expected) in [
        (SwitchKind::ToneControl, "on", "PSTONE%20CTRL%20ON"),
        (SwitchKind::ToneControl, "off", "PSTONE%20CTRL%20OFF"),
        (SwitchKind::DynamicEq, "on", "PSDYNEQ%20ON"),
        (SwitchKind::DynamicEq, "off", "PSDYNEQ%20OFF"),
        (SwitchKind::Trigger1, "on", "TR1%20ON"),
        (SwitchKind::Trigger2, "off", "TR2%20OFF"),
        (SwitchKind::Trigger3, "on", "TR3%20ON"),
        (SwitchKind::BluetoothTransmitter, "on", "BTTX%20ON"),
    ] {
        assert_eq!(
            vec![expected.to_owned()],
            sent(switch(kind), cmd, None).await,
            "{kind:?} {cmd}"
        );
    }
}

#[tokio::test]
async fn toggle_turns_an_unknown_switch_on() {
    let commands = sent(EntityKind::Switch(SwitchKind::DynamicEq), "toggle", None).await;
    assert_eq!(vec!["PSDYNEQ%20ON".to_owned()], commands);
}

#[tokio::test]
async fn the_bass_and_treble_numbers_send_levels() {
    let set = params(&json!({ "value": 2 }));
    let commands = sent(
        EntityKind::Number(NumberKind::Bass),
        "set_value",
        Some(&set),
    )
    .await;
    // The library turns the tone control on first.
    assert_eq!(vec!["PSTONE%20CTRL%20ON", "PSBAS%2052"], commands);
    // The level is 0 dB.
    let commands = sent(EntityKind::Number(NumberKind::Treble), "increment", None).await;
    assert_eq!(vec!["PSTONE%20CTRL%20ON", "PSTRE%2051"], commands);
    let high = params(&json!({ "value": 20 }));
    let commands = sent(
        EntityKind::Number(NumberKind::Bass),
        "set_value",
        Some(&high),
    )
    .await;
    assert_eq!(vec!["PSTONE%20CTRL%20ON", "PSBAS%2056"], commands);
}

#[tokio::test]
async fn the_extra_selects_send_their_commands() {
    let option = |text: &str| params(&json!({ "option": text }));
    let select = |kind| EntityKind::Select(kind);
    for (kind, text, expected) in [
        (SelectKind::MultiEq, "Reference", "PSMULTEQ:AUDYSSEY"),
        (SelectKind::BluetoothOutput, "Bluetooth Only", "BTTX%20BT"),
        (SelectKind::QuickSelect, "Quick Select 3", "MSQUICK3"),
    ] {
        let commands = sent(select(kind), "select_option", Some(&option(text))).await;
        assert_eq!(vec![expected.to_owned()], commands, "{kind:?} {text}");
    }
}

#[tokio::test]
async fn an_unknown_option_is_refused_and_nothing_is_sent() {
    let bad = params(&json!({ "option": "Nonsense" }));
    for kind in [
        SelectKind::MultiEq,
        SelectKind::TunerBand,
        SelectKind::BluetoothOutput,
        SelectKind::QuickSelect,
    ] {
        let (result, commands) =
            send(true, EntityKind::Select(kind), "select_option", Some(&bad)).await;
        assert!(
            matches!(result, Err(ServiceError::BadRequest(_))),
            "{kind:?}: {result:?}"
        );
        assert!(commands.is_empty(), "{kind:?}: {commands:?}");
    }
}

#[tokio::test]
async fn nothing_is_sent_while_the_receiver_is_off() {
    let set = params(&json!({ "value": 2 }));
    let option = params(&json!({ "option": "Reference" }));
    let cases: [Case<'_>; 3] = [
        (EntityKind::Switch(SwitchKind::ToneControl), "on", None),
        (
            EntityKind::Number(NumberKind::Bass),
            "set_value",
            Some(&set),
        ),
        (
            EntityKind::Select(SelectKind::MultiEq),
            "select_option",
            Some(&option),
        ),
    ];
    for (kind, cmd, params) in cases {
        let (result, commands) = send(false, kind, cmd, params).await;
        assert!(result.is_ok(), "{kind:?}: {result:?}");
        assert!(commands.is_empty(), "{kind:?}: {commands:?}");
    }
}

#[tokio::test]
async fn a_sensor_has_no_commands() {
    let (result, _) = send(
        true,
        EntityKind::Sensor(crate::entities::SensorKind::DiracFilter),
        "on",
        None,
    )
    .await;
    assert!(matches!(result, Err(ServiceError::NotSupported(_))));
}
