// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The actor of a receiver against a fake receiver: a fake HTTP server and a fake telnet port.
//! The tests check what the actor asks for and shows when the receiver answers, does not answer,
//! sends events or closes the connection.

use super::discovery::{TEST_BUILDER, TEST_DISCOVERY, TestBuilder};
use super::{Command, Connect, DenonActor, DeviceEvent, DeviceStatus, Disconnect};
use crate::config_store::DeviceConfig;
use crate::entities::{EntityKind, SwitchKind};
use crate::test_support::{Fake, FakeTelnet, test_builder};
use actix::{Actor, Addr, Context, Handler};
use denon_marantz::{Brand, DiscoveredReceiver, ReceiverType};
use serde_json::Value;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Collects what the actor sends to the controller.
struct Collector(Arc<Mutex<Vec<DeviceEvent>>>);

impl Actor for Collector {
    type Context = Context<Self>;
}

impl Handler<DeviceEvent> for Collector {
    type Result = ();

    fn handle(&mut self, event: DeviceEvent, _: &mut Context<Self>) {
        self.0.lock().expect("lock").push(event);
    }
}

struct Rig {
    actor: Addr<DenonActor>,
    events: Arc<Mutex<Vec<DeviceEvent>>>,
    http: Fake,
    telnet: FakeTelnet,
}

fn cfg() -> DeviceConfig {
    DeviceConfig {
        id: "SERIAL0000001".into(),
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
    }
}

/// An actor that is connected to a fake receiver. `answers` gives the lines of the telnet port for
/// a line of the actor.
async fn rig(
    answers: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
    retry: Duration,
) -> Rig {
    let http = Fake::start(true).await;
    let telnet = FakeTelnet::start(answers).await;
    let (http_port, telnet_port) = (http.port(), telnet.port);
    let setup: TestBuilder =
        Rc::new(move |_, builder| test_builder(http_port, telnet_port, builder));
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let actor = DenonActor::new("SERIAL0000001".into(), cfg(), collector.recipient())
        .with_retry_delay(retry)
        .start();
    actor.send(Connect).await.expect("connect");
    Rig {
        actor,
        events,
        http,
        telnet,
    }
}

/// Wait until `condition` is true. Fails the test after `secs` seconds.
async fn until(what: &str, secs: u64, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !condition() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The state of the media player that the actor sent last.
fn media_state(events: &Mutex<Vec<DeviceEvent>>) -> Option<Value> {
    let events = events.lock().expect("lock");
    events.iter().rev().find_map(|event| match event {
        DeviceEvent::Changes { changes, .. } => changes
            .iter()
            .find(|(kind, _)| *kind == EntityKind::MediaPlayer)
            .and_then(|(_, attrs)| attrs.get("state").cloned()),
        _ => None,
    })
}

/// Answers for the toggles of a receiver.
fn toggles(line: &str) -> Vec<String> {
    match line {
        "PSTONE CTRL ?" => vec!["PSTONE CTRL OFF".into()],
        "PSDYNEQ ?" => vec!["PSDYNEQ OFF".into()],
        "BTTX ?" => vec!["BTTX OFF".into()],
        "TR?" => vec!["TR1 OFF".into(), "TR2 OFF".into(), "TR3 OFF".into()],
        _ => Vec::new(),
    }
}

/// A receiver that answers every query. The library waits for the answer of a query before it
/// sends the next one: a receiver that stays silent makes the start very slow.
fn answers_all(line: &str) -> Vec<String> {
    let answer = toggles(line);
    // The library ends the wait for the answers of the start with a query for the power.
    if line == "PW?" {
        vec!["PWON".into()]
    } else if answer.is_empty() && line.ends_with('?') {
        vec!["MSSTEREO".into()]
    } else {
        answer
    }
}

/// A receiver that answers every query, except the one for the dynamic EQ.
fn leaves_out_the_dynamic_eq(line: &str) -> Vec<String> {
    if line == "PSDYNEQ ?" {
        Vec::new()
    } else {
        answers_all(line)
    }
}

/// A receiver with a Dirac filter selected: it ignores the tone control.
fn with_a_dirac_filter(line: &str) -> Vec<String> {
    if line == "PSDIRAC ?" {
        vec!["PSDIRAC 2".into()]
    } else {
        answers_all(line)
    }
}

const QUICK: Duration = Duration::from_millis(100);

#[actix::test]
async fn the_actor_asks_for_the_groups_that_the_entities_need() {
    let rig = rig(answers_all, QUICK).await;
    // Device: the sleep timer, the monitor output. Sound: the reference level. Picture: the mode.
    for query in ["SLP?", "VSMONI ?", "PSREFLEV ?", "PV?"] {
        until(query, 5, || rig.telnet.count(query) >= 1).await;
    }
}

/// Waits until the extra values of the start are asked: the picture mode is the last one.
async fn after_the_extra_values(rig: &Rig) {
    until("the picture mode", 10, || rig.telnet.count("PV?") >= 1).await;
}

#[actix::test]
async fn a_new_source_reads_only_the_digital_codec_again() {
    let rig = rig(answers_all, QUICK).await;
    after_the_extra_values(&rig).await;
    let (codec, monitor) = (rig.telnet.count("DC?"), rig.telnet.count("VSMONI ?"));
    rig.telnet.send("SIGAME");
    // The read comes after a short wait, so that a burst of events is read once.
    until("the second read", 5, || rig.telnet.count("DC?") > codec).await;
    assert_eq!(monitor, rig.telnet.count("VSMONI ?"));
}

#[actix::test]
async fn a_new_speaker_preset_reads_the_dirac_slots_and_the_values_that_depend_on_it() {
    let rig = rig(answers_all, QUICK).await;
    after_the_extra_values(&rig).await;
    let (slots, level) = (rig.telnet.count("OPDLS ?"), rig.telnet.count("PSREFLEV ?"));
    let sleep = rig.telnet.count("SLP?");
    rig.telnet.send("SPPR 2");
    until("the slots", 5, || rig.telnet.count("OPDLS ?") > slots).await;
    until("the level", 5, || rig.telnet.count("PSREFLEV ?") > level).await;
    assert_eq!(sleep, rig.telnet.count("SLP?"));
}

#[actix::test]
async fn a_filter_change_reads_the_values_that_it_blocks_but_not_the_slots() {
    let rig = rig(answers_all, QUICK).await;
    after_the_extra_values(&rig).await;
    let (slots, level) = (rig.telnet.count("OPDLS ?"), rig.telnet.count("PSREFLEV ?"));
    rig.telnet.send("PSDIRAC 2");
    until("the level", 5, || rig.telnet.count("PSREFLEV ?") > level).await;
    assert_eq!(slots, rig.telnet.count("OPDLS ?"));
}

#[actix::test]
async fn a_dynamic_eq_change_reads_only_the_reference_level() {
    let rig = rig(answers_all, QUICK).await;
    after_the_extra_values(&rig).await;
    let (level, volume) = (
        rig.telnet.count("PSREFLEV ?"),
        rig.telnet.count("PSDYNVOL ?"),
    );
    rig.telnet.send("PSDYNEQ ON");
    until("the level", 5, || rig.telnet.count("PSREFLEV ?") > level).await;
    assert_eq!(volume, rig.telnet.count("PSDYNVOL ?"));
}

/// The number of toggle queries once the start has sent its own queries: the number does not
/// change for a while.
async fn toggle_queries_after_the_start(rig: &Rig) -> usize {
    until("the first query", 5, || rig.telnet.count("PSDYNEQ ?") >= 1).await;
    let mut last = rig.telnet.count("PSDYNEQ ?");
    let mut since = Instant::now();
    while since.elapsed() < Duration::from_millis(1200) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = rig.telnet.count("PSDYNEQ ?");
        if now != last {
            last = now;
            since = Instant::now();
        }
    }
    last
}

#[actix::test]
async fn toggles_that_the_receiver_does_not_answer_are_asked_for_again_once() {
    // The retry comes 3 s after the answers of the start are in: the count of the start is stable
    // before.
    let rig = rig(leaves_out_the_dynamic_eq, Duration::from_secs(3)).await;
    let base = toggle_queries_after_the_start(&rig).await;
    until("the retry", 20, || rig.telnet.count("PSDYNEQ ?") > base).await;
    // Not more than one try.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(base + 1, rig.telnet.count("PSDYNEQ ?"));
}

#[actix::test]
async fn toggles_that_the_receiver_answers_are_not_asked_for_again() {
    let rig = rig(answers_all, Duration::from_secs(3)).await;
    let base = toggle_queries_after_the_start(&rig).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(base, rig.telnet.count("PSDYNEQ ?"));
}

#[actix::test]
async fn a_lost_telnet_connection_is_read_over_http_and_read_again_when_it_is_back() {
    let rig = rig(answers_all, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    let (reads, queries) = (rig.http.app_commands(), rig.telnet.count("SLP?"));
    rig.telnet.close();
    // While telnet is down, the state is read over HTTP.
    until("a read over HTTP", 5, || rig.http.app_commands() > reads).await;
    // The library connects again, and the driver reads all values once more.
    until("a new connection", 5, || rig.telnet.connections() >= 2).await;
    until("the read after the reconnect", 5, || {
        rig.telnet.count("SLP?") > queries
    })
    .await;
}

#[actix::test]
async fn a_pause_shows_paused_until_the_source_changes() {
    let rig = rig(answers_all, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    for line in ["PWON", "SINET", "NSE0Now Playing", "NSE1A Title"] {
        rig.telnet.send(line);
    }
    until("playing", 5, || {
        media_state(&rig.events) == Some("PLAYING".into())
    })
    .await;
    let command = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "play_pause".into(),
        params: None,
    };
    rig.actor.send(command).await.expect("send").expect("pause");
    until("paused", 5, || {
        media_state(&rig.events) == Some("PAUSED".into())
    })
    .await;
    rig.telnet.send("SIDVD");
    until("not paused", 5, || {
        media_state(&rig.events).is_some_and(|s| s != "PAUSED")
    })
    .await;
}

#[actix::test]
async fn a_command_that_the_state_of_the_receiver_does_not_allow_is_answered_with_ok() {
    let rig = rig(with_a_dirac_filter, QUICK).await;
    until("the filter", 5, || rig.telnet.count("PSDIRAC ?") >= 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let command = Command {
        kind: EntityKind::Switch(SwitchKind::ToneControl),
        cmd_id: "on".into(),
        params: None,
    };
    // The library refuses it and sends nothing. The Remote gets OK and shows no error.
    rig.actor.send(command).await.expect("send").expect("ok");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(0, rig.telnet.count("PSTONE CTRL ON"));
}

#[actix::test]
async fn the_start_asks_for_each_toggle_once() {
    // A long retry delay: only the queries of the start are counted.
    let rig = rig(answers_all, Duration::from_secs(60)).await;
    let count = toggle_queries_after_the_start(&rig).await;
    assert_eq!(1, count);
    assert_eq!(1, rig.telnet.count("PSTONE CTRL ?"));
}

#[actix::test]
async fn a_reconnect_asks_for_each_group_of_the_start_once() {
    let rig = rig(answers_all, Duration::from_secs(60)).await;
    let first = toggle_queries_after_the_start(&rig).await;
    assert_eq!(1, first);
    rig.telnet.close();
    until("a new connection", 5, || rig.telnet.connections() >= 2).await;
    // The library asks for its groups again. The driver adds its own groups, not these.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(2, rig.telnet.count("PSTONE CTRL ?"));
    assert_eq!(2, rig.telnet.count("PSDYNEQ ?"));
}

#[actix::test]
async fn a_command_sent_before_standby_reaches_the_receiver() {
    let rig = rig(answers_all, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    let command = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "off".into(),
        params: None,
    };
    // The disconnect comes while the command runs.
    let sent = rig.actor.send(command);
    rig.actor.do_send(Disconnect);
    sent.await.expect("send").expect("off");
    until("the command", 5, || rig.telnet.count("ZMOFF") == 1).await;
    until("the disconnect", 5, || {
        rig.events.lock().expect("lock").iter().any(|event| {
            matches!(
                event,
                DeviceEvent::Status {
                    status: DeviceStatus::Disconnected,
                    ..
                }
            )
        })
    })
    .await;
}

#[actix::test]
async fn a_receiver_with_a_new_address_is_found_by_its_serial_number() {
    const OLD: &str = "localhost";
    const NEW: &str = "127.0.0.1";
    let (old_http, old_telnet) = (
        Fake::start(true).await,
        FakeTelnet::start(answers_all).await,
    );
    let (new_http, new_telnet) = (
        Fake::start(true).await,
        FakeTelnet::start(answers_all).await,
    );
    let old = (old_http.port(), old_telnet.port);
    let new = (new_http.port(), new_telnet.port);
    let setup: TestBuilder = Rc::new(move |host, builder| {
        let (http, telnet) = if host == NEW { new } else { old };
        test_builder(http, telnet, builder)
    });
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let found = DiscoveredReceiver {
        host: NEW.into(),
        brand: Brand::Denon,
        manufacturer: "Denon".into(),
        model_name: None,
        friendly_name: None,
        serial_number: Some("SERIAL0000001".into()),
    };
    TEST_DISCOVERY.with(|slot| *slot.borrow_mut() = Some(vec![found]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let cfg = DeviceConfig {
        address: OLD.into(),
        ..cfg()
    };
    let actor = DenonActor::new("SERIAL0000001".into(), cfg, collector.recipient())
        .with_retry_delay(QUICK)
        .with_poll_interval(QUICK)
        .start();
    actor.send(Connect).await.expect("connect");
    until("the first read", 5, || old_telnet.count("SLP?") >= 1).await;

    old_http.leave();
    old_telnet.leave();
    // Three failed reads over HTTP, the old address again, then the search.
    let start = Instant::now();
    let found = || {
        events
            .lock()
            .expect("lock")
            .iter()
            .any(|event| matches!(event, DeviceEvent::Config { cfg, .. } if cfg.address == NEW))
    };
    while !found() {
        if start.elapsed() > Duration::from_secs(15) {
            let statuses: Vec<_> = events
                .lock()
                .expect("lock")
                .iter()
                .filter_map(|event| match event {
                    DeviceEvent::Status { status, .. } => Some(*status),
                    _ => None,
                })
                .collect();
            panic!(
                "no new address after 15 s. Statuses: {statuses:?}. Reads over HTTP: old {}, \
                new {}. Telnet connections: old {}, new {}",
                old_http.app_commands(),
                new_http.app_commands(),
                old_telnet.connections(),
                new_telnet.connections()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    until("the new connection", 5, || new_telnet.connections() >= 1).await;
}

/// The last state of an entity that the actor sent.
fn entity_state(events: &Mutex<Vec<DeviceEvent>>, kind: EntityKind, attr: &str) -> Option<Value> {
    let events = events.lock().expect("lock");
    events.iter().rev().find_map(|event| match event {
        DeviceEvent::Changes { changes, .. } => changes
            .iter()
            .find(|(k, _)| *k == kind)
            .and_then(|(_, attrs)| attrs.get(attr).cloned()),
        _ => None,
    })
}

// A receiver with Dirac Live and without an Audyssey calibration answers the Audyssey values
// blank over HTTP. The values that telnet gave must stay.
#[actix::test]
async fn audyssey_values_stay_known_after_a_blank_http_answer() {
    let http = Fake::start(true).await;
    let telnet = FakeTelnet::start(answers_all).await;
    let (http_port, telnet_port) = (http.port(), telnet.port);
    let setup: TestBuilder = Rc::new(move |_, builder| {
        test_builder(http_port, telnet_port, builder).probes(&[(ReceiverType::AvrX2016, http_port)])
    });
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    // No second query for the toggles during the test.
    let actor = DenonActor::new("SERIAL0000001".into(), cfg(), collector.recipient())
        .with_retry_delay(Duration::from_secs(60))
        .start();
    actor.send(Connect).await.expect("connect");
    until("the extra values", 10, || telnet.count("PV?") >= 1).await;
    // The fake answers over HTTP after the telnet answers.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let kind = EntityKind::Switch(SwitchKind::DynamicEq);
    assert_eq!(
        Some(Value::from("OFF")),
        entity_state(&events, kind, "state")
    );
}
