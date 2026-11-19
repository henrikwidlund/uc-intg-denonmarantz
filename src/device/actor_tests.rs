// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! The actor of a receiver against a fake receiver: a fake HTTP server and a fake telnet port.
//! The tests check what the actor asks for and shows when the receiver answers, does not answer,
//! sends events or closes the connection.

use super::actor::{CurrentAvr, read_differently, send_command};
use super::commands;
use super::discovery::{self, Known, TEST_BUILDER, TEST_DISCOVERY, TestBuilder};
use super::heos::TEST_HEOS;
use super::{Command, Connect, DenonActor, DeviceEvent, DeviceStatus, Disconnect, UpdateConfig};
use crate::config_store::{self, CommApiVersion, DeviceConfig};
use crate::entities::sensor::EMPTY;
use crate::entities::{EntityKind, SelectKind, SensorKind, SwitchKind};
use crate::errors::ServiceError;
use crate::test_support::{Fake, FakeHeos, FakeTelnet, HeosItem, quick_http, test_builder};
use actix::{Actor, Addr, Context, Handler};
use denon_marantz::{Brand, DiscoveredReceiver, Info, ReceiverType, TypeProof, UpdateMethod};
use serde_json::Value;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// The MAC address and serial number of the fake receiver.
const FAKE_MAC: &str = "0123456789AB";
const FAKE_SERIAL: &str = "XXX";

struct Rig {
    actor: Addr<DenonActor>,
    events: Arc<Mutex<Vec<DeviceEvent>>>,
    http: Fake,
    telnet: FakeTelnet,
}

fn cfg() -> DeviceConfig {
    DeviceConfig {
        id: "SERIAL0000001".into(),
        id_kind: config_store::IdKind::Serial,
        serial: None,
        // The MAC address of the fake receiver.
        mac: Some(FAKE_MAC.into()),
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
        hdmi_output_over_http: None,
        quick_select_count: None,
        listed_queries: None,
        comm_api_version: None,
        has_tuner: None,
        tuner_presets_lettered: None,
        info: None,
    }
}

/// An actor that is connected to a fake receiver. `answers` gives the lines of the telnet port for
/// a line of the actor.
async fn rig(
    answers: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
    retry: Duration,
) -> Rig {
    rig_with(|_| cfg(), answers, retry).await
}

/// As [`rig`], with the configuration that `cfg` gives for the HTTP port of the fake receiver.
async fn rig_with(
    cfg: impl FnOnce(u16) -> DeviceConfig,
    answers: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
    retry: Duration,
) -> Rig {
    rig_on(Fake::start(true).await, cfg, answers, retry).await
}

/// As [`rig_with`], with this fake receiver.
async fn rig_on(
    http: Fake,
    cfg: impl FnOnce(u16) -> DeviceConfig,
    answers: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
    retry: Duration,
) -> Rig {
    let telnet = FakeTelnet::start(answers).await;
    let (http_port, telnet_port) = (http.port(), telnet.port);
    let setup: TestBuilder =
        Rc::new(move |_, builder| test_builder(http_port, telnet_port, builder));
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let actor = DenonActor::new(
        "SERIAL0000001".into(),
        cfg(http_port),
        collector.recipient(),
    )
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

/// How often the actor has sent this status.
fn status_count(events: &Mutex<Vec<DeviceEvent>>, wanted: DeviceStatus) -> usize {
    events
        .lock()
        .expect("lock")
        .iter()
        .filter(|event| matches!(event, DeviceEvent::Status { status, .. } if *status == wanted))
        .count()
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
async fn the_values_of_older_receivers_are_read_only_on_older_receivers() {
    let rig = rig(answers_all, QUICK).await;
    after_the_extra_values(&rig).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for line in ["PSDIL ?", "PSMODE: ?", "MNPRV?", "NSH"] {
        assert_eq!(0, rig.telnet.count(line), "{line}");
    }
    // The tuner presets are asked on all receivers that may have a tuner.
    until("the tuner presets", 5, || rig.telnet.count("OPTPN ?") == 1).await;

    let http = Fake::start_with_info(
        b"<Device_Info><CommApiVers>0300</CommApiVers><ModelName>AVR-X2100W</ModelName>\
        <DeviceZones>2</DeviceZones><MacAddress>0123456789AB</MacAddress></Device_Info>",
    )
    .await;
    let old = rig_on(http, |_| cfg(), answers_all, QUICK).await;
    for line in [
        "PSDIL ?",
        "PSMODE: ?",
        "PSDSX ?",
        "MNPRV?",
        "NSH",
        "OPTPN ?",
    ] {
        until(line, 10, || old.telnet.count(line) >= 1).await;
    }
    // Not in the spec of 2014 and 2015.
    assert_eq!(0, old.telnet.count("PSSB: ?"));

    // A saved favorite has a new name: the names are read again.
    let names = old.telnet.count("NSH");
    let save = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "NSC02".into(),
        params: None,
    };
    old.actor.send(save).await.expect("send").expect("save");
    until("the names again", 5, || old.telnet.count("NSH") > names).await;
    let presets = old.telnet.count("OPTPN ?");
    let save = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "TPANMEM03".into(),
        params: None,
    };
    old.actor.send(save).await.expect("send").expect("save");
    until("the presets again", 5, || {
        old.telnet.count("OPTPN ?") > presets
    })
    .await;

    // A new name is sent. Other changes do not send the select with its 56 options again.
    let favorite = EntityKind::Select(SelectKind::NetworkFavorite);
    old.telnet.send("NSH02Jazz");
    until("the new name", 5, || {
        entity_state(&old.events, favorite, "options")
            .is_some_and(|o| o.to_string().contains("3 Jazz"))
    })
    .await;
    let before = old.events.lock().expect("lock").len();
    old.telnet.send("SLP010");
    let sleep = EntityKind::Sensor(SensorKind::SleepTimer);
    until("the sleep timer", 5, || {
        entity_state(&old.events, sleep, "value") == Some(10.into())
    })
    .await;
    let events = old.events.lock().expect("lock");
    let sent = events[before..].iter().any(|event| {
        matches!(event, DeviceEvent::Changes { changes, .. } if changes.iter().any(|(k, _)| *k == favorite))
    });
    assert!(!sent);
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
async fn a_disconnect_drops_the_receiver_at_once() {
    // A toggle has no answer: the start waits 60 s before it asks again.
    let rig = rig(leaves_out_the_dynamic_eq, Duration::from_secs(60)).await;
    toggle_queries_after_the_start(&rig).await;
    let avr = rig
        .actor
        .send(CurrentAvr)
        .await
        .expect("send")
        .expect("avr");
    rig.actor.send(Disconnect).await.expect("disconnect");
    until("the receiver is dropped", 3, || avr.upgrade().is_none()).await;
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
    assert_eq!(
        1,
        count,
        "connections: {}, lines: {:?}",
        rig.telnet.connections(),
        rig.telnet.received()
    );
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
    // A failed connect before can have sent this status already.
    let before = status_count(&rig.events, DeviceStatus::Disconnected);
    // The disconnect comes while the command runs.
    let sent = rig.actor.send(command);
    rig.actor.do_send(Disconnect);
    sent.await.expect("send").expect("off");
    until("the command", 5, || rig.telnet.count("ZMOFF") == 1).await;
    until("the disconnect", 5, || {
        status_count(&rig.events, DeviceStatus::Disconnected) > before
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
        mac_address: None,
        type_proof: TypeProof::Read,
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
/// A receiver that answers every query with a line that does not change the sound mode.
fn keeps_the_sound_mode(line: &str) -> Vec<String> {
    let mut answer = answers_all(line);
    for text in &mut answer {
        if text == "MSSTEREO" {
            "MVMAX 98".clone_into(text);
        }
    }
    answer
}

#[actix::test]
async fn a_receiver_without_a_sound_mode_list_shows_the_groups_after_the_start() {
    let rig = rig(answers_all, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    rig.telnet.send("PWON");
    rig.telnet.send("MSSTEREO");
    let select = EntityKind::Select(SelectKind::SoundMode);
    until("the group", 5, || {
        entity_state(&rig.events, select, "current_option") == Some("STEREO".into())
    })
    .await;
}

// After a change, the receiver reports a mode that is not in its list for about 1 s.
#[actix::test]
async fn the_sound_mode_select_keeps_the_last_mode_that_is_in_the_list() {
    let rig = rig(keeps_the_sound_mode, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    for line in [
        "PWON",
        "OPSMLALL MOV001Multi Ch In",
        "OPSMLALL MOV002Direct",
        "OPSMLALL END",
        "MSMULTI CH IN",
    ] {
        rig.telnet.send(line);
    }
    let select = EntityKind::Select(SelectKind::SoundMode);
    until("multi ch in", 5, || {
        entity_state(&rig.events, select, "current_option") == Some("Multi Ch In".into())
    })
    .await;
    let before = rig.events.lock().expect("lock").len();
    rig.telnet.send("MSDOLBY ATMOS");
    // The first send after this one that shows the new mode in the sensor.
    let sensor = EntityKind::Sensor(SensorKind::SoundMode);
    let shown = || {
        let events = rig.events.lock().expect("lock");
        events[before..].iter().find_map(|event| match event {
            DeviceEvent::Changes { changes, .. } => {
                let attr = |kind, name| {
                    changes
                        .iter()
                        .find(|(k, _)| *k == kind)
                        .and_then(|(_, attrs)| attrs.get(name).cloned())
                };
                (attr(sensor, "value") == Some("DOLBY ATMOS".into()))
                    .then(|| attr(select, "current_option"))
            }
            _ => None,
        })
    };
    until("the new mode", 5, || shown().is_some()).await;
    assert_eq!(Some(Some("Multi Ch In".into())), shown());
}

#[actix::test]
async fn a_new_sound_mode_list_drops_a_kept_mode_that_is_not_in_it() {
    let rig = rig(keeps_the_sound_mode, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    for line in [
        "PWON",
        "OPSMLALL MOV001Multi Ch In",
        "OPSMLALL MOV002Direct",
        "OPSMLALL END",
        "MSMULTI CH IN",
    ] {
        rig.telnet.send(line);
    }
    let select = EntityKind::Select(SelectKind::SoundMode);
    let mode = || entity_state(&rig.events, select, "current_option");
    until("multi ch in", 5, || mode() == Some("Multi Ch In".into())).await;
    // A list without Multi Ch In: no option fits the reported mode.
    for line in [
        "OPSMLALL MOV001Stereo",
        "OPSMLALL MOV002Direct",
        "OPSMLALL END",
    ] {
        rig.telnet.send(line);
    }
    until("no option", 5, || mode() == Some(EMPTY.into())).await;
}

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

/// An actor that is connected to a fake receiver whose telnet port refuses connects. Gives the
/// actor, the fake receiver and the telnet port. Start the fake telnet port on it later.
async fn connected_without_telnet() -> (Addr<DenonActor>, Fake, u16) {
    let http = Fake::start(true).await;
    // A free port that refuses connects until the fake telnet port starts on it.
    let telnet_port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    let http_port = http.port();
    let setup: TestBuilder =
        Rc::new(move |_, builder| test_builder(http_port, telnet_port, builder));
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let actor = DenonActor::new("SERIAL0000001".into(), cfg(), collector.recipient())
        .with_retry_delay(Duration::from_secs(60))
        .start();
    actor.send(Connect).await.expect("connect");
    until("connected", 5, || {
        status_count(&events, DeviceStatus::Connected) == 1
    })
    .await;
    (actor, http, telnet_port)
}

// The library does not try again when the start cannot open telnet, for example while the network
// of the Remote is not ready after wake-up.
#[actix::test]
async fn telnet_opens_when_the_port_answers_after_the_start() {
    let (_actor, _http, telnet_port) = connected_without_telnet().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let telnet = FakeTelnet::start_at(telnet_port, answers_all).await;
    // The fast tries run every 50 ms.
    until("telnet", 1, || telnet.connections() >= 1).await;
    until("the extra values", 5, || telnet.count("PV?") >= 1).await;
}

#[actix::test]
async fn standby_stops_the_telnet_tries() {
    let (actor, _http, telnet_port) = connected_without_telnet().await;
    actor.send(Disconnect).await.expect("disconnect");
    let telnet = FakeTelnet::start_at(telnet_port, answers_all).await;
    // The fast tries would connect within 50 ms.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(0, telnet.connections());
}

#[actix::test]
async fn the_connect_adds_the_receiver_kind_to_the_configuration() {
    let rig = rig(answers_all, QUICK).await;
    let reported = || {
        rig.events.lock().expect("lock").iter().any(|event| {
            matches!(event, DeviceEvent::Config { cfg, .. }
                if cfg.receiver_type == Some(config_store::ReceiverGeneration::AvrX)
                    && cfg.update_method == Some(config_store::HttpReadMethod::AppCommand))
        })
    };
    until("the receiver kind", 5, reported).await;
}

#[actix::test]
async fn a_connect_during_the_disconnect_wait_keeps_the_connection_like_a_new_one() {
    let http = Fake::start(true).await;
    let http_port = http.port();
    let setup: TestBuilder = Rc::new(move |_, builder| test_builder(http_port, 1, builder));
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let cfg = DeviceConfig {
        use_telnet: false,
        ..cfg()
    };
    let actor = DenonActor::new("SERIAL0000001".into(), cfg, collector.recipient())
        .with_poll_interval(QUICK)
        .start();
    actor.send(Connect).await.expect("connect");
    // The connect also reads the receiver: the poll is checked after it.
    until("the connect", 5, || {
        status_count(&events, DeviceStatus::Connected) > 0
    })
    .await;
    let connected = http.app_commands();
    until("the first poll", 5, || http.app_commands() > connected).await;
    let command = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "off".into(),
        params: None,
    };
    let disconnects = status_count(&events, DeviceStatus::Disconnected);
    // The disconnect waits for the command, and the connect comes during that wait.
    let sent = actor.send(command);
    actor.do_send(Disconnect);
    actor.do_send(Connect);
    sent.await.expect("send").expect("off");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        status_count(&events, DeviceStatus::Disconnected) == disconnects,
        "the connection was closed"
    );
    let before = http.app_commands();
    until("more polls", 5, || http.app_commands() >= before + 3).await;
    // A failed read shows the entities as unavailable before the driver searches the receiver
    // again.
    http.leave();
    until("unavailable", 5, || {
        media_state(&events) == Some(Value::from("UNAVAILABLE"))
    })
    .await;
    assert!(
        status_count(&events, DeviceStatus::Disconnected) == disconnects,
        "the entities were not unavailable before the search"
    );
}

#[actix::test]
async fn the_http_poll_reads_audyssey_and_the_signal() {
    let http = Fake::start(true).await;
    let http_port = http.port();
    let setup: TestBuilder = Rc::new(move |_, builder| test_builder(http_port, 1, builder));
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let cfg = DeviceConfig {
        use_telnet: false,
        ..cfg()
    };
    let actor = DenonActor::new("SERIAL0000001".into(), cfg, collector.recipient())
        .with_poll_interval(QUICK)
        .start();
    actor.send(Connect).await.expect("connect");
    until("the first poll", 5, || http.app_commands() >= 2).await;
    let before = http.audyssey_reads();
    until("two more reads of Audyssey", 5, || {
        http.audyssey_reads() >= before + 2
    })
    .await;
}

/// A port that holds the first connection without an answer and passes the others to `target`.
async fn hang_first(target: u16) -> u16 {
    hang_first_n(target, 1).await.0
}

/// A port that holds the first `n` connections without an answer and passes the others to
/// `target`. Also gives the number of connections so far.
async fn hang_first_n(target: u16, n: usize) -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("address").port();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        let mut held = Vec::with_capacity(n.min(8));
        while let Ok((socket, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::Relaxed);
            if held.len() < n {
                held.push(socket);
                continue;
            }
            tokio::spawn(forward(socket, target));
        }
    });
    (port, connections)
}

/// Pass a connection on to the port `target`.
async fn forward(mut socket: tokio::net::TcpStream, target: u16) {
    if let Ok(mut fake) = tokio::net::TcpStream::connect(("127.0.0.1", target)).await {
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut fake).await;
    }
}

/// Builders for the HTTP port `port`. A request waits longer than [`discovery::STALL_MIN`], so that
/// the time limit of the connect ends it first.
fn stalling_builder(port: u16) {
    let setup: TestBuilder = Rc::new(move |_, builder| {
        test_builder(port, 1, builder).http_config(denon_marantz::HttpConfig {
            request_timeout: Duration::from_secs(5),
            ..quick_http()
        })
    });
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
}

#[actix::test]
async fn a_stalled_connect_is_started_again() {
    let http = Fake::start(true).await;
    let port = hang_first(http.port()).await;
    stalling_builder(port);
    let mut stall = discovery::Stall::new(Some(Duration::from_millis(10)));
    let start = Instant::now();
    let deadline = start + discovery::NETWORK_RETRY_TIMEOUT;
    let avr = discovery::identify_retry("127.0.0.1", None, deadline, &mut stall)
        .await
        .expect("identify");
    assert!(start.elapsed() >= discovery::STALL_MIN);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(ReceiverType::AvrX, avr.info().receiver_type);
}

#[actix::test]
async fn a_stalled_start_is_started_again() {
    let http = Fake::start(true).await;
    let port = hang_first(http.port()).await;
    let avr = Arc::new(avr_on(&http, port, Duration::ZERO).await);
    let mut stall = discovery::Stall::new(Some(Duration::from_millis(10)));
    let start = Instant::now();
    let deadline = start + discovery::NETWORK_RETRY_TIMEOUT;
    let (_started, took) = discovery::start_retry(&avr, false, deadline, &mut stall)
        .await
        .expect("start");
    assert!(start.elapsed() >= discovery::STALL_MIN);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(took < discovery::STALL_MIN, "{took:?}");
}

#[actix::test]
async fn a_connect_without_a_short_last_time_has_no_limit() {
    let http = Fake::start(true).await;
    for last in [None, Some(Duration::from_millis(700))] {
        let (port, connections) = hang_first_n(http.port(), usize::MAX).await;
        stalling_builder(port);
        let mut stall = discovery::Stall::new(last);
        let deadline = Instant::now() + discovery::NETWORK_RETRY_TIMEOUT;
        let identify = discovery::identify_retry("127.0.0.1", None, deadline, &mut stall);
        // The first connection gets no answer: without a limit, the identify waits.
        let waited = tokio::time::timeout(Duration::from_secs(1), identify).await;
        assert!(waited.is_err(), "{last:?}");
        // Not started again.
        assert_eq!(1, connections.load(Ordering::Relaxed), "{last:?}");
    }
}

#[actix::test]
async fn a_stalled_connect_is_started_again_twice_at_most() {
    let http = Fake::start(true).await;
    let (port, connections) = hang_first_n(http.port(), 3).await;
    stalling_builder(port);
    let mut stall = discovery::Stall::new(Some(Duration::from_millis(10)));
    let deadline = Instant::now() + discovery::NETWORK_RETRY_TIMEOUT;
    let identify = discovery::identify_retry("127.0.0.1", None, deadline, &mut stall);
    // Two restarts after 0.2 s each, then the third try has no limit and waits.
    let waited = tokio::time::timeout(Duration::from_secs(2), identify).await;
    assert!(waited.is_err());
    // The first try and two restarts.
    assert_eq!(3, connections.load(Ordering::Relaxed));
}

#[actix::test]
async fn the_stored_brand_is_used_if_the_description_names_none() {
    let http = Fake::start_without_brand().await;
    let http_port = http.port();
    let setup: TestBuilder = Rc::new(move |_, builder| test_builder(http_port, 1, builder));
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let marantz = DeviceConfig {
        is_denon: false,
        ..cfg()
    };
    let avr = discovery::identify("127.0.0.1", Some(Known::of(&marantz)))
        .await
        .expect("identify");
    assert_eq!(Brand::Marantz, avr.info().brand);
    let avr = discovery::identify("127.0.0.1", None)
        .await
        .expect("identify");
    assert_eq!(Brand::Denon, avr.info().brand);
    let stored = DeviceConfig {
        receiver_type: Some(config_store::ReceiverGeneration::AvrX2016),
        ..cfg()
    };
    assert_eq!(
        Some(ReceiverType::AvrX2016),
        Known::of(&stored).receiver_type
    );
    let oldest = DeviceConfig {
        receiver_type: Some(config_store::ReceiverGeneration::Avr),
        ..cfg()
    };
    assert_eq!(None, Known::of(&oldest).receiver_type);
}

#[actix::test]
async fn a_changed_receiver_type_is_looked_up_again() {
    let http = Fake::start(true).await;
    let http_port = http.port();
    let builders = Rc::new(std::cell::Cell::new(0));
    let counted = builders.clone();
    // The fake is an AVR-X receiver.
    let setup: TestBuilder = Rc::new(move |_, builder| {
        counted.set(counted.get() + 1);
        test_builder(http_port, 1, builder)
    });
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let known = |receiver_type| Known {
        receiver_type: Some(receiver_type),
        brand: Brand::Denon,
    };

    let avr = discovery::identify("127.0.0.1", Some(known(ReceiverType::AvrX)))
        .await
        .expect("identify");
    assert_eq!(ReceiverType::AvrX, avr.info().receiver_type);
    assert_eq!(1, builders.get());

    let avr = discovery::identify("127.0.0.1", Some(known(ReceiverType::AvrX2016)))
        .await
        .expect("identify");
    assert_eq!(ReceiverType::AvrX, avr.info().receiver_type);
    assert_eq!(3, builders.get());
}

#[actix::test]
async fn an_error_answer_does_not_make_a_newer_receiver_the_oldest_type() {
    let http = Fake::start_busy().await;
    let http_port = http.port();
    // Both types on the port of the fake: the error answer gives the oldest type there.
    let setup: TestBuilder = Rc::new(move |_, builder| {
        test_builder(http_port, 1, builder).probes(&[
            (ReceiverType::AvrX, http_port),
            (ReceiverType::Avr, http_port),
        ])
    });
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    let known = |receiver_type| Known {
        receiver_type: Some(receiver_type),
        brand: Brand::Denon,
    };

    let error = discovery::identify("127.0.0.1", Some(known(ReceiverType::AvrX)))
        .await
        .expect_err("a newer receiver must not be the oldest type");
    // Not a network error: the connect is not retried at once.
    assert!(!error.is_unreachable(), "{error}");

    // A stored `Avr` is looked up each time. It stays `Avr` while the receiver gives no device
    // information.
    let oldest = DeviceConfig {
        receiver_type: Some(config_store::ReceiverGeneration::Avr),
        ..cfg()
    };
    let avr = discovery::identify("127.0.0.1", Some(Known::of(&oldest)))
        .await
        .expect("identify");
    assert_eq!(ReceiverType::Avr, avr.info().receiver_type);
    let avr = discovery::identify("127.0.0.1", None)
        .await
        .expect("identify");
    assert_eq!(ReceiverType::Avr, avr.info().receiver_type);
}

#[actix::test]
async fn a_type_that_is_not_proven_is_not_saved() {
    // A configuration of the Python integration: no type yet. The receiver answers the device
    // information with an error.
    let cfg = DeviceConfig {
        id: FAKE_SERIAL.into(),
        mac: None,
        ..cfg()
    };
    let http = Fake::start_busy().await;
    let http_port = http.port();
    // Both types on the port of the fake: the error answer gives the oldest type there.
    let setup: TestBuilder = Rc::new(move |_, builder| {
        test_builder(http_port, 1, builder).probes(&[
            (ReceiverType::AvrX, http_port),
            (ReceiverType::Avr, http_port),
        ])
    });
    let (_actor, events, _http) = connect_http_only_to(http, setup, cfg).await;
    until("the connect", 5, || {
        status_count(&events, DeviceStatus::Connected) > 0
    })
    .await;
    let events = events.lock().expect("lock");
    let configs: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            DeviceEvent::Config { cfg, .. } => Some(cfg),
            _ => None,
        })
        .collect();
    // The serial number is added, the type is not.
    assert_ne!(0, configs.len());
    assert!(
        configs
            .iter()
            .all(|cfg| cfg.receiver_type.is_none() && cfg.update_method.is_none())
    );
}

/// An actor for this configuration that is connected to a fake receiver whose telnet port does
/// not answer. The search finds no receiver.
async fn connect_http_only(
    cfg: DeviceConfig,
) -> (Addr<DenonActor>, Arc<Mutex<Vec<DeviceEvent>>>, Fake) {
    let http = Fake::start(true).await;
    let http_port = http.port();
    let setup: TestBuilder = Rc::new(move |_, builder| test_builder(http_port, 1, builder));
    connect_http_only_to(http, setup, cfg).await
}

async fn connect_http_only_to(
    http: Fake,
    setup: TestBuilder,
    cfg: DeviceConfig,
) -> (Addr<DenonActor>, Arc<Mutex<Vec<DeviceEvent>>>, Fake) {
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    TEST_DISCOVERY.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let cfg = DeviceConfig {
        use_telnet: false,
        ..cfg
    };
    let actor = DenonActor::new(cfg.id.as_str().into(), cfg, collector.recipient())
        .with_retry_delay(Duration::from_secs(60))
        .start();
    actor.send(Connect).await.expect("connect");
    (actor, events, http)
}

#[actix::test]
async fn the_connect_adds_the_mac_address_to_a_configuration_with_a_serial_number() {
    // A configuration of the Python integration: the serial number is the id.
    let cfg = DeviceConfig {
        id: FAKE_SERIAL.into(),
        mac: None,
        ..cfg()
    };
    let (_actor, events, _http) = connect_http_only(cfg).await;
    let added = || {
        events.lock().expect("lock").iter().any(|event| {
            matches!(event, DeviceEvent::Config { cfg, .. }
                if cfg.id == FAKE_SERIAL
                    && cfg.id_kind == config_store::IdKind::Serial
                    && cfg.mac.as_deref() == Some(FAKE_MAC)
                    && cfg.serial.as_deref() == Some(FAKE_SERIAL))
        })
    };
    until("the MAC address", 5, added).await;
}

#[actix::test]
async fn another_receiver_at_the_stored_address_is_not_used() {
    let cfg = DeviceConfig {
        mac: Some("AABBCCDDEEFF".into()),
        ..cfg()
    };
    let (_actor, events, _http) = connect_http_only(cfg).await;
    // The connect ends without a connection.
    let statuses = || -> Vec<DeviceStatus> {
        events
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|event| match event {
                DeviceEvent::Status { status, .. } => Some(*status),
                _ => None,
            })
            .collect()
    };
    until("the failed connect", 5, || {
        statuses()
            .iter()
            .skip_while(|s| **s != DeviceStatus::Connecting)
            .any(|s| *s != DeviceStatus::Connecting)
    })
    .await;
    assert!(!statuses().contains(&DeviceStatus::Connected));
}

#[actix::test]
async fn repeat_and_shuffle_go_to_heos() {
    let heos = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
    TEST_HEOS.with(|slot| slot.set(Some(heos.config())));
    let rig = rig(answers_all, QUICK).await;
    until("HEOS asked", 5, || heos.count("player/get_players") > 0).await;
    rig.telnet.send("PWON");
    rig.telnet.send("SINET");
    let attr = |name| entity_state(&rig.events, EntityKind::MediaPlayer, name);
    until("the HEOS title", 5, || {
        attr("media_title") == Some("A Song".into())
    })
    .await;
    assert_eq!(Some(Value::from("OFF")), attr("repeat"));
    assert_eq!(Some(Value::from(false)), attr("shuffle"));

    let send = |cmd_id: &str, params: Value| Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: cmd_id.into(),
        params: params.as_object().cloned(),
    };
    let repeat = send("repeat", serde_json::json!({"repeat": "ALL"}));
    rig.actor.send(repeat).await.expect("send").expect("repeat");
    until("repeat all", 5, || attr("repeat") == Some("ALL".into())).await;
    // Shuffle keeps the repeat mode.
    let shuffle = send("shuffle", serde_json::json!({"shuffle": true}));
    rig.actor
        .send(shuffle)
        .await
        .expect("send")
        .expect("shuffle");
    until("shuffle on", 5, || attr("shuffle") == Some(true.into())).await;
    let modes: Vec<String> = heos
        .received()
        .into_iter()
        .filter(|l| l.starts_with("player/set_play_mode"))
        .collect();
    assert_eq!(
        vec![
            "player/set_play_mode?pid=7&repeat=on_all&shuffle=off",
            "player/set_play_mode?pid=7&repeat=on_all&shuffle=on",
        ],
        modes
    );

    // Without HEOS in use, the command does nothing and the answer is OK.
    rig.telnet.send("SIDVD");
    until("DVD", 5, || attr("media_title") == Some("DVD".into())).await;
    let off = send("repeat", serde_json::json!({"repeat": "OFF"}));
    rig.actor.send(off).await.expect("send").expect("ignored");
    assert_eq!(2, heos.count("player/set_play_mode"));
}

#[actix::test]
async fn the_media_player_shows_heos_on_network_audio() {
    let heos = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
    TEST_HEOS.with(|slot| slot.set(Some(heos.config())));
    let rig = rig(answers_all, QUICK).await;
    until("HEOS asked", 5, || heos.count("player/get_players") > 0).await;
    rig.telnet.send("PWON");
    rig.telnet.send("SINET");
    let title = || entity_state(&rig.events, EntityKind::MediaPlayer, "media_title");
    until("the HEOS title", 5, || title() == Some("A Song".into())).await;
    assert_eq!(Some("PLAYING".into()), media_state(&rig.events));

    let command = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "play_pause".into(),
        params: None,
    };
    rig.actor.send(command).await.expect("send").expect("pause");
    assert!(
        heos.received()
            .iter()
            .any(|l| l == "player/set_play_state?pid=7&state=pause")
    );
    heos.play(HeosItem {
        state: "pause",
        ..HeosItem::song()
    });
    until("paused", 5, || {
        media_state(&rig.events) == Some("PAUSED".into())
    })
    .await;

    // Another source: HEOS is not shown.
    rig.telnet.send("SIDVD");
    until("the source name", 5, || title() == Some("DVD".into())).await;
    assert_eq!(Some("ON".into()), media_state(&rig.events));
}

#[actix::test]
async fn the_comm_api_version_is_saved() {
    let rig = rig(answers_all, QUICK).await;
    let saved = || {
        rig.events.lock().expect("lock").iter().any(|event| {
            matches!(event, DeviceEvent::Config { cfg, .. }
                if cfg.comm_api_version == CommApiVersion::parse("0301"))
        })
    };
    until("the version saved", 5, saved).await;
}

#[actix::test]
async fn the_form_of_the_tuner_presets_is_saved() {
    let rig = rig(answers_all, QUICK).await;
    after_the_extra_values(&rig).await;
    rig.telnet.send("TPANA1");
    let saved = || {
        rig.events.lock().expect("lock").iter().any(|event| {
            matches!(event, DeviceEvent::Config { cfg, .. } if cfg.tuner_presets_lettered == Some(true))
        })
    };
    until("the form saved", 5, saved).await;
}

#[actix::test]
async fn heos_is_not_asked_below_comm_api_version_0301() {
    let heos = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
    TEST_HEOS.with(|slot| slot.set(Some(heos.config())));
    let http = Fake::start_with_info(
        b"<Device_Info><CommApiVers>0300</CommApiVers><ModelName>AVR-X2100W</ModelName>\
        <DeviceZones>2</DeviceZones><MacAddress>0123456789AB</MacAddress></Device_Info>",
    )
    .await;
    let rig = rig_on(http, |_| cfg(), answers_all, QUICK).await;
    until("connected", 5, || {
        rig.events.lock().expect("lock").iter().any(|event| {
            matches!(event, DeviceEvent::Status { status, .. } if *status == DeviceStatus::Connected)
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(Vec::<String>::new(), heos.received());
}

#[actix::test]
async fn heos_without_the_player_is_used_when_the_player_comes() {
    let heos = FakeHeos::start("192.0.2.1", HeosItem::song()).await;
    TEST_HEOS.with(|slot| slot.set(Some(heos.config())));
    let rig = rig(answers_all, QUICK).await;
    until("HEOS asked", 5, || heos.count("player/get_players") > 0).await;
    rig.telnet.send("PWON");
    rig.telnet.send("SINET");
    let reads = heos.count("player/get_players");
    until("read again", 5, || heos.count("player/get_players") > reads).await;
    heos.move_player("127.0.0.1");
    let title = || entity_state(&rig.events, EntityKind::MediaPlayer, "media_title");
    until("the HEOS title", 5, || title() == Some("A Song".into())).await;
}

#[actix::test]
async fn a_heos_command_without_an_answer_does_not_hold_up_the_receiver_commands() {
    let heos = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
    // The HEOS client waits longer than the driver.
    let patient = denon_marantz_heos::HeosConfig {
        command_timeout: Duration::from_secs(20),
        slow_command_limit: Duration::from_secs(20),
        ..heos.config()
    };
    TEST_HEOS.with(|slot| slot.set(Some(patient)));
    let rig = rig(answers_all, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    rig.telnet.send("PWON");
    rig.telnet.send("SINET");
    let title = || entity_state(&rig.events, EntityKind::MediaPlayer, "media_title");
    until("the HEOS title", 5, || title() == Some("A Song".into())).await;
    heos.hang();
    let command = |cmd_id: &str| Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: cmd_id.into(),
        params: None,
    };
    let start = Instant::now();
    let pause = async {
        let result = rig.actor.send(command("play_pause")).await;
        (result, start.elapsed())
    };
    let mute = async {
        let result = rig.actor.send(command("mute_toggle")).await;
        (result, start.elapsed())
    };
    let ((pause, paused_after), (mute, muted_after)) = tokio::join!(pause, mute);
    mute.expect("send").expect("mute");
    assert!(muted_after < Duration::from_secs(2), "{muted_after:?}");
    // The driver gives up after 5 s.
    assert_eq!(Err(ServiceError::Timeout), pause.expect("send"));
    assert!(
        (Duration::from_secs(5)..Duration::from_secs(7)).contains(&paused_after),
        "{paused_after:?}"
    );
    until("mute sent", 2, || rig.telnet.count("MUON") >= 1).await;
}

#[actix::test]
async fn the_heos_client_is_kept_while_the_receiver_connects_again() {
    let heos = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
    TEST_HEOS.with(|slot| slot.set(Some(heos.config())));
    let rig = rig(answers_all, QUICK).await;
    until("HEOS connected", 5, || {
        heos.count("player/get_players") >= 1
    })
    .await;
    let reads = heos.count("player/get_players");
    let connected = status_count(&rig.events, DeviceStatus::Connected);
    // A new poll setting connects to the receiver again, as after a lost connection.
    let cfg = DeviceConfig {
        signal_poll_secs: 5,
        ..cfg()
    };
    rig.actor.send(UpdateConfig(cfg)).await.expect("update");
    until("connected again", 5, || {
        status_count(&rig.events, DeviceStatus::Connected) > connected
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(reads, heos.count("player/get_players"));
}

#[actix::test]
async fn the_heos_client_is_new_after_a_disconnect() {
    let heos = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
    TEST_HEOS.with(|slot| slot.set(Some(heos.config())));
    let rig = rig(answers_all, QUICK).await;
    until("HEOS connected", 5, || {
        heos.count("player/get_players") >= 1
    })
    .await;
    let reads = heos.count("player/get_players");
    rig.actor.send(Disconnect).await.expect("disconnect");
    until("disconnected", 5, || {
        status_count(&rig.events, DeviceStatus::Disconnected) >= 1
    })
    .await;
    let connected = status_count(&rig.events, DeviceStatus::Connected);
    rig.actor.send(Connect).await.expect("connect");
    until("connected again", 5, || {
        status_count(&rig.events, DeviceStatus::Connected) > connected
    })
    .await;
    // At standby, the HEOS connections can break without notice: new ones are opened.
    until("HEOS connected again", 5, || {
        heos.count("player/get_players") > reads
    })
    .await;
}

/// The receiver values that the actor stored last.
fn stored_info(events: &Mutex<Vec<DeviceEvent>>) -> Option<Info> {
    events
        .lock()
        .expect("lock")
        .iter()
        .rev()
        .find_map(|event| match event {
            DeviceEvent::Config { cfg, .. } => cfg.info.clone(),
            _ => None,
        })
}

/// The receiver values of the fake receiver, from a connect that looks them up.
async fn fake_info() -> Info {
    let rig = rig(answers_all, QUICK).await;
    until("the stored values", 5, || {
        stored_info(&rig.events).is_some()
    })
    .await;
    stored_info(&rig.events).expect("info")
}

#[actix::test]
async fn a_connect_stores_the_receiver_values_and_the_next_one_uses_them() {
    let rig = rig(answers_all, QUICK).await;
    until("the stored values", 5, || {
        stored_info(&rig.events).is_some()
    })
    .await;
    let reads = rig.http.description_reads();
    assert!(reads >= 1);
    let stored = rig
        .events
        .lock()
        .expect("lock")
        .iter()
        .rev()
        .find_map(|event| match event {
            DeviceEvent::Config { cfg, .. } if cfg.info.is_some() => Some(cfg.clone()),
            _ => None,
        });
    let stored = stored.expect("config");
    let text = serde_json::to_string(&stored).expect("json");
    assert_eq!(stored, serde_json::from_str(&text).expect("read"));
    // A failed connect before can have sent this status already.
    let before = status_count(&rig.events, DeviceStatus::Disconnected);
    rig.actor.send(Disconnect).await.expect("disconnect");
    until("the disconnect", 5, || {
        status_count(&rig.events, DeviceStatus::Disconnected) > before
    })
    .await;
    let connected = status_count(&rig.events, DeviceStatus::Connected);
    rig.actor.send(Connect).await.expect("connect");
    until("the new connection", 5, || {
        status_count(&rig.events, DeviceStatus::Connected) > connected
    })
    .await;
    assert_eq!(reads, rig.http.description_reads());
}

#[actix::test]
async fn changed_receiver_values_are_stored_without_a_new_connect() {
    let mut info = fake_info().await;
    let name = info.friendly_name.clone();
    info.friendly_name = "Old name".into();
    let cfg = |port| DeviceConfig {
        info: Some(Info { port, ..info }),
        ..cfg()
    };
    let rig = rig_with(cfg, answers_all, QUICK).await;
    until("the new values", 5, || {
        stored_info(&rig.events).is_some_and(|i| i.friendly_name == name)
    })
    .await;
    assert_eq!(0, rig.http.description_reads());
    assert_eq!(1, status_count(&rig.events, DeviceStatus::Connected));
}

#[actix::test]
async fn changed_receiver_values_are_stored_before_the_start_ends() {
    let mut info = fake_info().await;
    let name = info.friendly_name.clone();
    info.friendly_name = "Old name".into();
    let cfg = |port| DeviceConfig {
        info: Some(Info { port, ..info }),
        ..cfg()
    };
    let rig = rig_with(cfg, answers_all, QUICK).await;
    until("the new values", 5, || {
        stored_info(&rig.events).is_some_and(|i| i.friendly_name == name)
    })
    .await;
    // The start reads the signal after the check. The fake answers that read late.
    assert_eq!(0, rig.http.audyssey_answers());
}

#[actix::test]
async fn another_receiver_at_the_stored_address_is_looked_up() {
    const OTHER_MAC: &str = "AABBCCDDEEFF";
    let mut info = fake_info().await;
    info.mac_address = Some(OTHER_MAC.into());
    let cfg = |port| DeviceConfig {
        mac: Some(OTHER_MAC.into()),
        info: Some(Info { port, ..info }),
        ..cfg()
    };
    TEST_DISCOVERY.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
    let rig = rig_with(cfg, answers_all, QUICK).await;
    until("the look-up", 5, || rig.http.description_reads() >= 1).await;
    until("the lost connection", 5, || {
        status_count(&rig.events, DeviceStatus::Disconnected) >= 1
    })
    .await;
    assert!(
        stored_info(&rig.events).is_none_or(|i| i.mac_address.as_deref() == Some(OTHER_MAC)),
        "the values of the other receiver are not stored"
    );
}

#[actix::test]
async fn a_failed_connect_with_the_stored_values_looks_the_receiver_up_at_once() {
    let mut info = fake_info().await;
    // A port where nothing answers.
    info.port = 9;
    let cfg = |_| DeviceConfig {
        info: Some(info),
        ..cfg()
    };
    let rig = rig_with(cfg, answers_all, Duration::from_secs(60)).await;
    until("the connection", 10, || {
        status_count(&rig.events, DeviceStatus::Connected) == 1
    })
    .await;
    assert!(rig.http.description_reads() >= 1);
}

/// A media player command without parameters.
fn media_command(cmd_id: &str) -> Command {
    Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: cmd_id.into(),
        params: None,
    }
}

/// A rig whose receiver values are stored, connected again with a start that reads slowly. Also
/// gives how often it was connected before.
async fn slow_start_rig() -> (Rig, usize) {
    let rig = rig(answers_all, QUICK).await;
    until("the stored values", 5, || {
        stored_info(&rig.events).is_some()
    })
    .await;
    // A failed connect before can have sent this status already.
    let before = status_count(&rig.events, DeviceStatus::Disconnected);
    rig.actor.send(Disconnect).await.expect("disconnect");
    until("the disconnect", 5, || {
        status_count(&rig.events, DeviceStatus::Disconnected) > before
    })
    .await;
    // Below the request timeout of the tests.
    rig.http.set_read_delay(Duration::from_millis(300));
    let connected = status_count(&rig.events, DeviceStatus::Connected);
    rig.actor.send(Connect).await.expect("connect");
    (rig, connected)
}

#[actix::test]
async fn a_command_that_does_not_read_the_state_is_sent_before_the_start_ends() {
    let (rig, connected) = slow_start_rig().await;
    rig.actor
        .send(media_command("volume_up"))
        .await
        .expect("send")
        .expect("volume up");
    assert_eq!(
        connected,
        status_count(&rig.events, DeviceStatus::Connected)
    );
    assert!(
        rig.http.commands().iter().any(|c| c == "MVUP"),
        "{:?}",
        rig.http.commands()
    );
}

/// A free port. It refuses connections until `open` passes them to `target`.
async fn closed_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    listener.local_addr().expect("address").port()
}

/// Pass the connections to `port` on to `target`.
async fn open(port: u16, target: u16) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind again");
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(forward(socket, target));
        }
    });
}

/// A receiver whose HTTP port is `port`. The connect sends no request. A connection that cannot
/// be built is tried again for `retry`.
async fn avr_on(fake: &Fake, port: u16, retry: Duration) -> denon_marantz::Avr {
    let info = Info {
        port,
        ..fake.avr().await.info().clone()
    };
    denon_marantz::Avr::builder("127.0.0.1")
        .http_config(denon_marantz::HttpConfig {
            connect_retry: retry,
            ..denon_marantz::HttpConfig::default()
        })
        .known(info)
        .connect()
        .await
        .expect("connect")
}

#[actix::test]
async fn a_command_before_the_start_ends_is_sent_once_the_network_is_up() {
    let fake = Fake::start(true).await;
    let port = closed_port().await;
    let (cfg, expected) = (cfg(), commands::Expected::default());
    let msg = media_command("volume_down");
    let at_once = avr_on(&fake, port, Duration::ZERO).await;
    let failed = send_command(&at_once, None, &cfg, &expected, &msg, None, "AVR").await;
    assert!(
        matches!(failed, Err(ServiceError::ServiceUnavailable(_))),
        "{failed:?}"
    );
    let avr = avr_on(&fake, port, Duration::from_secs(3)).await;
    let (_attempts, attempt) = tokio::sync::watch::channel(1_u64);
    let send = send_command(&avr, None, &cfg, &expected, &msg, Some(attempt), "AVR");
    let opened = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        open(port, fake.port()).await;
    };
    let (result, ()) = tokio::join!(send, opened);
    result.expect("volume down");
    assert!(
        fake.commands().iter().any(|c| c == "MVDOWN"),
        "{:?}",
        fake.commands()
    );
}

#[actix::test]
async fn a_command_is_not_sent_after_a_new_wakeup() {
    let fake = Fake::start(true).await;
    let port = closed_port().await;
    let avr = avr_on(&fake, port, Duration::from_secs(3)).await;
    let (cfg, expected) = (cfg(), commands::Expected::default());
    let msg = media_command("volume_down");
    let (attempts, attempt) = tokio::sync::watch::channel(1_u64);
    let start = Instant::now();
    let send = send_command(&avr, None, &cfg, &expected, &msg, Some(attempt), "AVR");
    let stopped = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Standby closes the connection and the next wake-up connects again.
        attempts.send_modify(|a| *a += 1);
        attempts.send_modify(|a| *a += 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The receiver can be reached again: a command that still runs gets there.
        open(port, fake.port()).await;
    };
    let (result, ()) = tokio::join!(send, stopped);
    // The user cannot do anything about it: no error.
    assert_eq!(Ok(()), result);
    assert!(start.elapsed() < Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(fake.commands().is_empty(), "{:?}", fake.commands());
}

#[actix::test]
async fn a_command_after_one_that_waits_for_the_state_waits_too() {
    let (rig, _) = slow_start_rig().await;
    let toggle = rig.actor.send(media_command("mute_toggle"));
    let up = rig.actor.send(media_command("volume_up"));
    toggle.await.expect("send").expect("mute toggle");
    up.await.expect("send").expect("volume up");
    let sent = || -> Vec<String> {
        rig.http
            .commands()
            .into_iter()
            .chain(rig.telnet.received())
            .filter(|line| line.starts_with("MU") || line == "MVUP")
            .collect()
    };
    // Telnet commands are queued: the receiver gets them a little later.
    until("both commands", 5, || sent().len() >= 2).await;
    let sent = sent();
    let mute = sent.iter().position(|l| l.starts_with("MU"));
    let up = sent.iter().position(|l| l == "MVUP");
    assert!(mute.is_some() && up.is_some() && mute < up, "{sent:?}");
}

#[actix::test]
async fn another_receiver_is_looked_up_on_each_try_until_it_is_found() {
    const OTHER_MAC: &str = "AABBCCDDEEFF";
    let mut info = fake_info().await;
    info.mac_address = Some(OTHER_MAC.into());
    let cfg = |port| DeviceConfig {
        mac: Some(OTHER_MAC.into()),
        info: Some(Info { port, ..info }),
        ..cfg()
    };
    TEST_DISCOVERY.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
    let rig = rig_with(cfg, answers_all, QUICK).await;
    // The first connect uses the stored values. Each try after it looks the receiver up.
    until("three look-ups", 15, || rig.http.description_reads() >= 3).await;
    assert_eq!(1, status_count(&rig.events, DeviceStatus::Connected));
}

#[actix::test]
async fn changed_values_update_the_receiver_kind_without_a_new_connect() {
    let info = fake_info().await;
    let count = info.quick_select_count;
    let cfg = |port| DeviceConfig {
        info: Some(Info {
            port,
            quick_select_count: Some(99),
            ..info
        }),
        quick_select_count: Some(99),
        ..cfg()
    };
    let rig = rig_with(cfg, answers_all, QUICK).await;
    let updated = || {
        rig.events.lock().expect("lock").iter().any(|event| {
            matches!(event, DeviceEvent::Config { cfg, .. }
                if cfg.quick_select_count == count
                    && cfg.info.as_ref().is_some_and(|i| i.quick_select_count == count))
        })
    };
    until("the new receiver kind", 5, updated).await;
    assert_eq!(1, status_count(&rig.events, DeviceStatus::Connected));
}

#[actix::test]
async fn only_a_new_type_port_or_update_method_connects_again() {
    let info = fake_info().await;
    let renamed = Info {
        friendly_name: "Other name".into(),
        ..info.clone()
    };
    assert!(!read_differently(&info, &renamed));
    let changed = [
        Info {
            port: 1,
            ..info.clone()
        },
        Info {
            receiver_type: ReceiverType::AvrX2016,
            ..info.clone()
        },
        Info {
            update_method: UpdateMethod::StatusPages,
            ..info.clone()
        },
    ];
    for new in &changed {
        assert!(read_differently(&info, new), "{new:?}");
    }
}
