// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Entity commands against a fake receiver: the commands that the library sends.

use super::Command;
use super::commands::{Expected, run, run_steps};
use crate::config_store::DeviceConfig;
use crate::entities::{EntityKind, NumberKind, SelectKind, SwitchKind, remote};
use crate::errors::ServiceError;
use crate::test_support::Fake;
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::watch;

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
    let msg = Command::new(kind, cmd_id.into(), params.cloned());
    let result = run(&avr, None, &cfg(), &Expected::default(), &msg).await;
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
        &Command::new(EntityKind::MediaPlayer, "stop".into(), None),
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

/// A run in these tests ends well before this time. A run that does not end fails the test.
const RUN_LIMIT: Duration = Duration::from_secs(10);

/// A run of 3 `cursor_up` steps with `delay_ms` between them.
fn cursor_up_run(delay_ms: u64) -> remote::RemoteRun {
    let p = json!({ "sequence": ["cursor_up"], "repeat": 3, "delay": delay_ms });
    remote::command("send_cmd_sequence", Some(&params(&p))).expect("run")
}

/// Run three steps while the lock is held by another command. `end` runs while the run waits for
/// the lock, then the lock is free. The sender that `end` returns lives until the run ends.
/// Returns the result of the run and the commands that the receiver got.
async fn steps_after_wait(
    end: impl FnOnce(watch::Sender<()>) -> Option<watch::Sender<()>>,
) -> (Option<u64>, Vec<String>) {
    let fake = Fake::start(true).await;
    let avr = fake.avr().await;
    let run = cursor_up_run(0);
    let lock = tokio::sync::Mutex::new(());
    let (stop_tx, stop) = watch::channel(());
    let (cfg, expected) = (cfg(), Expected::default());
    let held = lock.lock().await;
    let steps = run_steps(&avr, None, &cfg, &expected, &run, &lock, stop);
    let release = async {
        tokio::task::yield_now().await;
        let kept = end(stop_tx);
        drop(held);
        kept
    };
    let (result, _kept) = tokio::time::timeout(RUN_LIMIT, async { tokio::join!(steps, release) })
        .await
        .expect("the run ends");
    (result.expect("the run works"), fake.commands())
}

#[tokio::test]
async fn a_run_sends_all_steps_after_the_lock_is_free() {
    let (stopped, commands) = steps_after_wait(Some).await;
    assert_eq!(None, stopped);
    assert_eq!(3, commands.len(), "{commands:?}");
}

#[tokio::test]
async fn a_stop_while_the_run_waits_for_the_lock_sends_nothing() {
    let result = steps_after_wait(|stop| {
        stop.send_replace(());
        Some(stop)
    })
    .await;
    assert_eq!((Some(0), Vec::new()), result);
}

#[tokio::test]
async fn a_run_ends_when_the_stop_sender_is_dropped() {
    assert_eq!((Some(0), Vec::new()), steps_after_wait(|_| None).await);
}

#[tokio::test]
async fn a_stop_before_the_subscribe_does_not_stop_the_run() {
    let (stop_tx, _) = watch::channel(());
    stop_tx.send_replace(());
    let stop = stop_tx.subscribe();
    let fake = Fake::start(true).await;
    let avr = fake.avr().await;
    let run = cursor_up_run(0);
    let lock = tokio::sync::Mutex::new(());
    let (cfg, expected) = (cfg(), Expected::default());
    let steps = run_steps(&avr, None, &cfg, &expected, &run, &lock, stop);
    let stopped = tokio::time::timeout(RUN_LIMIT, steps)
        .await
        .expect("the run ends")
        .expect("the run works");
    assert_eq!(None, stopped);
    assert_eq!(3, fake.commands().len());
}

#[tokio::test]
async fn a_stop_during_the_delay_ends_the_run_at_once() {
    let fake = Fake::start(true).await;
    let avr = fake.avr().await;
    let run = cursor_up_run(5000);
    let lock = tokio::sync::Mutex::new(());
    let (stop_tx, stop) = watch::channel(());
    let (cfg, expected) = (cfg(), Expected::default());
    let start = std::time::Instant::now();
    let steps = run_steps(&avr, None, &cfg, &expected, &run, &lock, stop);
    let stopper = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while fake.commands().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the first step");
        // The first step was received. A stop during the step or the delay ends the run before the
        // next step.
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop_tx.send_replace(());
    };
    let (result, ()) = tokio::join!(steps, stopper);
    assert_eq!(Some(1), result.expect("the run works"));
    assert_eq!(1, fake.commands().len());
    assert!(start.elapsed() < Duration::from_secs(4));
}
