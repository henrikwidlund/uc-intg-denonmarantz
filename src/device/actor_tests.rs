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
use crate::test_support::{
    Fake, FakeHeos, FakeTelnet, FakeWeb, HeosItem, quick_http, test_builder,
};
use actix::{Actor, Addr, Context, Handler};
use denon_marantz::proto::telnet::answer_prefix;
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

/// A receiver that answers every query: a list with its `END` line, any other query with a line
/// that starts like its answer. The start waits for the answers: a query without one counts as not
/// answered only after `answer_quiet` of `test_builder`.
fn answers_all(line: &str) -> Vec<String> {
    let answer = toggles(line);
    if !answer.is_empty() || !line.ends_with('?') {
        return answer;
    }
    let prefix = answer_prefix(line);
    match prefix {
        "PW" => vec!["PWON".into()],
        "MS" => vec!["MSSTEREO".into()],
        "CV" | "SSAEQLRS" => vec![format!("{prefix}END")],
        "OPSMLALL" | "OPDLS" | "SSFUN" | "SSSOD" | "SSTTR" | "SSCFR" | "SSLEV" | "SSQSNZMA" => {
            vec![format!("{prefix} END")]
        }
        _ => vec![prefix.to_owned()],
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
    for line in ["PSDIL ?", "PSMODE: ?", "MNPRV?"] {
        assert_eq!(0, rig.telnet.count(line), "{line}");
    }

    let old = rig_on(older_receiver().await, |_| cfg(), answers_all, QUICK).await;
    for line in ["PSDIL ?", "PSMODE: ?", "PSDSX ?"] {
        until(line, 10, || old.telnet.count(line) >= 1).await;
    }
    // Not in the spec of 2014 and 2015.
    for line in ["PSSB: ?", "MNPRV?"] {
        assert_eq!(0, old.telnet.count(line), "{line}");
    }
}

/// A current receiver whose `Deviceinfo.xml` lists only the LFE level.
async fn receiver_with_few_features() -> Fake {
    Fake::start_with_info(
        b"<Device_Info><CommApiVers>0301</CommApiVers><ModelName>AVR-X3700H</ModelName>\
        <DeviceZones>2</DeviceZones><MacAddress>0123456789AB</MacAddress><DeviceCapabilities>\
        <Setup><SurroundParameter><LFE/></SurroundParameter></Setup></DeviceCapabilities>\
        </Device_Info>",
    )
    .await
}

#[actix::test]
async fn queries_of_features_that_the_receiver_does_not_list_are_not_sent() {
    let rig = rig_on(
        receiver_with_few_features().await,
        |_| cfg(),
        answers_all,
        QUICK,
    )
    .await;
    // The sleep timer needs no feature. The extra values are sent in one go.
    until("the sleep timer", 10, || rig.telnet.count("SLP?") >= 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for line in ["PV?", "PSIMAX ?", "SSSMG ?"] {
        assert_eq!(0, rig.telnet.count(line), "{line}");
    }
}

/// A Denon receiver of 2015 (`CommApiVers` 0300), with network favorites before HEOS.
async fn older_receiver() -> Fake {
    Fake::start_with_info(
        b"<Device_Info><CommApiVers>0300</CommApiVers><ModelName>AVR-X2100W</ModelName>\
        <DeviceZones>2</DeviceZones><MacAddress>0123456789AB</MacAddress></Device_Info>",
    )
    .await
}

/// The last names that the actor stored.
fn stored(events: &Mutex<Vec<DeviceEvent>>) -> (Vec<String>, Vec<String>) {
    let events = events.lock().expect("lock");
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            DeviceEvent::Config { cfg, .. } => Some((
                cfg.tuner_preset_names.clone(),
                cfg.network_favorite_names.clone(),
            )),
            _ => None,
        })
        .unwrap_or_default()
}

#[actix::test]
async fn names_are_asked_only_while_they_are_used_and_then_stored() {
    let rig = rig_on(older_receiver().await, |_| cfg(), answers_all, QUICK).await;
    after_the_extra_values(&rig).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(0, rig.telnet.count("OPTPN ?"));
    assert_eq!(0, rig.telnet.count("NSH"));

    // Not in standby: the receiver may not answer. Asked when it turns on.
    rig.telnet.send("ZMOFF");
    rig.telnet.send("SITUNER");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(0, rig.telnet.count("OPTPN ?"));
    rig.telnet.send("ZMON");
    until("the tuner presets", 5, || rig.telnet.count("OPTPN ?") == 1).await;

    // Once per connection, while the tuner or network audio plays.
    rig.telnet.send("SINET");
    until("the favorites", 5, || rig.telnet.count("NSH") == 1).await;
    rig.telnet.send("SITUNER");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(1, rig.telnet.count("OPTPN ?"));

    // The names are stored when the answers stop. The select shows the stored names.
    for line in [
        "OPTPN01KEXP     009250",
        "OPTPN02         010110",
        "NSH00Radio 1",
        "NSH01",
    ] {
        rig.telnet.send(line);
    }
    until("the stored names", 5, || {
        stored(&rig.events)
            == (
                vec!["KEXP".to_owned(), "101.10 MHz".to_owned()],
                vec!["Radio 1".to_owned(), String::new()],
            )
    })
    .await;
    let favorite = EntityKind::Select(SelectKind::NetworkFavorite);
    until("the stored name in the select", 5, || {
        entity_state(&rig.events, favorite, "options") == Some(serde_json::json!(["1 Radio 1"]))
    })
    .await;

    // Another preset: only the current option, not the 56 options.
    let preset = EntityKind::Select(SelectKind::TunerPreset);
    let before = rig.events.lock().expect("lock").len();
    rig.telnet.send("TPAN02");
    until("the current preset", 5, || {
        entity_state(&rig.events, preset, "current_option") == Some("2 101.10 MHz".into())
    })
    .await;
    let options_of = |from: usize| {
        let events = rig.events.lock().expect("lock");
        events[from..].iter().any(|event| {
            matches!(event, DeviceEvent::Changes { changes, .. }
                if changes.iter().any(|(k, attrs)| *k == preset && attrs.contains_key("options")))
        })
    };
    assert!(!options_of(before));
    // The options are empty while the receiver is off, and come back when it turns on.
    rig.telnet.send("ZMOFF");
    until("no options", 5, || {
        entity_state(&rig.events, preset, "options") == Some(serde_json::json!([EMPTY]))
    })
    .await;
    rig.telnet.send("ZMON");
    until("the options again", 5, || {
        entity_state(&rig.events, preset, "options")
            == Some(serde_json::json!(["1 KEXP", "2 101.10 MHz"]))
    })
    .await;

    // A saved favorite has a new name: the names are read again.
    let names = rig.telnet.count("NSH");
    let save = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "NSC01".into(),
        params: None,
    };
    rig.actor.send(save).await.expect("send").expect("save");
    until("the names again", 5, || rig.telnet.count("NSH") > names).await;
    let presets = rig.telnet.count("OPTPN ?");
    let save = Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: "TPANMEM03".into(),
        params: None,
    };
    rig.actor.send(save).await.expect("send").expect("save");
    until("the presets again", 5, || {
        rig.telnet.count("OPTPN ?") > presets
    })
    .await;

    // A new name is sent. Other changes do not build the options of the select again.
    rig.telnet.send("NSH01Jazz");
    until("the new name", 5, || {
        entity_state(&rig.events, favorite, "options")
            .is_some_and(|o| o.to_string().contains("2 Jazz"))
    })
    .await;
    let before = rig.events.lock().expect("lock").len();
    rig.telnet.send("SLP010");
    let sleep = EntityKind::Sensor(SensorKind::SleepTimer);
    until("the sleep timer", 5, || {
        entity_state(&rig.events, sleep, "value") == Some(10.into())
    })
    .await;
    let events = rig.events.lock().expect("lock");
    let built = events[before..].iter().any(|event| {
        matches!(event, DeviceEvent::Changes { changes, .. }
            if changes.iter().any(|(k, attrs)| *k == favorite && attrs.contains_key("options")))
    });
    assert!(!built);
}

#[actix::test]
async fn a_new_configuration_keeps_the_names_that_the_actor_learned() {
    let learned = vec!["Radio 1".to_owned()];
    let first = DeviceConfig {
        network_favorite_names: learned.clone(),
        tuner_presets_lettered: Some(true),
        ..cfg()
    };
    let rig = rig_with(move |_| first, answers_all, QUICK).await;
    until("the stored values", 5, || {
        stored_info(&rig.events).is_some()
    })
    .await;
    let count = configs(&rig.events).len();
    // A setup with older names. Another poll time connects again, and the connect saves the
    // receiver values.
    let setup = DeviceConfig {
        network_favorite_names: vec!["Old".into()],
        signal_poll_secs: 5,
        ..cfg()
    };
    rig.actor.send(UpdateConfig(setup)).await.expect("update");
    until("saved again", 5, || configs(&rig.events).len() > count).await;
    let saved = configs(&rig.events).pop().expect("saved");
    assert_eq!(
        (learned, Some(true)),
        (saved.network_favorite_names, saved.tuner_presets_lettered)
    );
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

/// The number of dynamic EQ queries once the start has sent its own queries: the number does not
/// change for a while.
async fn dynamic_eq_queries_after_the_start(rig: &Rig) -> usize {
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
async fn queries_that_the_receiver_does_not_answer_are_asked_for_again_once() {
    // The retry comes 3 s after the answers of the start are in: the count of the start is stable
    // before.
    let rig = rig(leaves_out_the_dynamic_eq, Duration::from_secs(3)).await;
    let base = dynamic_eq_queries_after_the_start(&rig).await;
    until("the retry", 20, || rig.telnet.count("PSDYNEQ ?") > base).await;
    // Not more than one try.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(base + 1, rig.telnet.count("PSDYNEQ ?"));
}

#[actix::test]
async fn a_disconnect_drops_the_receiver_at_once() {
    // A query has no answer: the start waits 60 s before it asks again.
    let rig = rig(leaves_out_the_dynamic_eq, Duration::from_secs(60)).await;
    dynamic_eq_queries_after_the_start(&rig).await;
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
async fn queries_that_the_receiver_answers_are_not_asked_for_again() {
    let rig = rig(answers_all, Duration::from_secs(3)).await;
    let base = dynamic_eq_queries_after_the_start(&rig).await;
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

/// The number of entity updates that the actor sent.
fn changes_count(events: &Mutex<Vec<DeviceEvent>>) -> usize {
    events
        .lock()
        .expect("lock")
        .iter()
        .filter(|event| matches!(event, DeviceEvent::Changes { .. }))
        .count()
}

/// Wait until the actor sent no entity update for 500 ms.
async fn until_quiet(events: &Mutex<Vec<DeviceEvent>>) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let before = changes_count(events);
        tokio::time::sleep(Duration::from_millis(500)).await;
        if changes_count(events) == before {
            return;
        }
        assert!(Instant::now() < end, "the actor does not stop sending");
    }
}

#[actix::test]
async fn a_command_sends_only_the_media_player_and_only_when_the_pause_changes() {
    let rig = rig(answers_all, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    for line in ["PWON", "SINET", "NSE0Now Playing", "NSE1A Title"] {
        rig.telnet.send(line);
    }
    until("playing", 5, || {
        media_state(&rig.events) == Some("PLAYING".into())
    })
    .await;
    // The new source reads the digital codec again later. Its answer sends all entities.
    until("the codec after the source", 5, || {
        rig.telnet.count("DC?") >= 2
    })
    .await;
    until_quiet(&rig.events).await;
    let command = |cmd_id: &str| Command {
        kind: EntityKind::MediaPlayer,
        cmd_id: cmd_id.into(),
        params: None,
    };
    // The receiver does not answer the cursor: nothing changed, so nothing is sent.
    let before = changes_count(&rig.events);
    rig.actor
        .send(command("cursor_up"))
        .await
        .expect("send")
        .expect("cursor");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(before, changes_count(&rig.events));
    // The pause is shown with the media player alone.
    rig.actor
        .send(command("play_pause"))
        .await
        .expect("send")
        .expect("pause");
    until("paused", 5, || {
        media_state(&rig.events) == Some("PAUSED".into())
    })
    .await;
    let events = rig.events.lock().expect("lock");
    let Some(DeviceEvent::Changes { changes, .. }) = events.last() else {
        panic!("no entity update");
    };
    let kinds: Vec<EntityKind> = changes.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(vec![EntityKind::MediaPlayer], kinds);
}

#[actix::test]
async fn stop_send_stops_a_running_sequence() {
    let rig = rig(answers_all, QUICK).await;
    until("the first read", 5, || rig.telnet.count("SLP?") >= 1).await;
    rig.telnet.send("PWON");
    let remote = |cmd_id: &str, params: Option<Value>| Command {
        kind: EntityKind::Remote,
        cmd_id: cmd_id.into(),
        params: params.and_then(|p| p.as_object().cloned()),
    };
    let params = serde_json::json!({ "sequence": ["cursor_up"], "repeat": 10, "delay": 200 });
    // The run is answered at once.
    rig.actor
        .send(remote("send_cmd_sequence", Some(params)))
        .await
        .expect("send")
        .expect("run");
    until("the first step", 5, || rig.telnet.count("MNCUP") >= 1).await;
    rig.actor
        .send(remote("stop_send", None))
        .await
        .expect("send")
        .expect("stop");
    let stopped = rig.telnet.count("MNCUP");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(rig.telnet.count("MNCUP") <= stopped + 1, "the run goes on");
    assert!(rig.telnet.count("MNCUP") < 10);
}

#[actix::test]
async fn a_repeated_toggle_switches_each_time() {
    // The receiver does not answer the change: only the run knows what it sent.
    let rig = rig(answers_all, QUICK).await;
    until("the dynamic EQ", 5, || rig.telnet.count("PSDYNEQ ?") >= 1).await;
    rig.telnet.send("PWON");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let params = serde_json::json!({ "command": "DYNAMIC_EQ_TOGGLE", "repeat": 2 });
    let command = Command {
        kind: EntityKind::Remote,
        cmd_id: "send_cmd".into(),
        params: params.as_object().cloned(),
    };
    rig.actor.send(command).await.expect("send").expect("run");
    until("both steps", 5, || {
        rig.telnet.count("PSDYNEQ ON") + rig.telnet.count("PSDYNEQ OFF") >= 2
    })
    .await;
    assert_eq!(1, rig.telnet.count("PSDYNEQ ON"));
    assert_eq!(1, rig.telnet.count("PSDYNEQ OFF"));
}

#[actix::test]
async fn stop_send_is_answered_while_the_receiver_is_not_connected() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events).start();
    let actor = DenonActor::new("SERIAL0000001".into(), cfg(), collector.recipient()).start();
    let stop = Command {
        kind: EntityKind::Remote,
        cmd_id: "stop_send".into(),
        params: None,
    };
    actor.send(stop).await.expect("send").expect("stop");
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
    let count = dynamic_eq_queries_after_the_start(&rig).await;
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
    let first = dynamic_eq_queries_after_the_start(&rig).await;
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
    // The address and the receiver values are saved together.
    let first = configs(&events)
        .into_iter()
        .find(|c| c.address == NEW)
        .expect("saved");
    assert!(first.info.is_some_and(|i| i.host == NEW));
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
    // No second query during the test.
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

#[test]
fn a_slow_connect_does_not_raise_the_limit_of_the_next_one() {
    let ms = Duration::from_millis;
    let mut times = discovery::ConnectTimes::default();
    assert_eq!(None, times.fastest());
    times.add(ms(20));
    // Slow wake-ups.
    times.add(ms(450));
    times.add(ms(640));
    assert_eq!(Some(ms(20)), times.fastest());
    // The fast time is replaced after 5 newer ones.
    for _ in 0..4 {
        times.add(ms(300));
    }
    assert_eq!(Some(ms(300)), times.fastest());
}

#[actix::test]
async fn a_stalled_connect_is_started_again_four_times_at_most() {
    let http = Fake::start(true).await;
    let (port, connections) = hang_first_n(http.port(), 5).await;
    stalling_builder(port);
    let mut stall = discovery::Stall::new(Some(Duration::from_millis(10)));
    let deadline = Instant::now() + discovery::NETWORK_RETRY_TIMEOUT;
    let identify = discovery::identify_retry("127.0.0.1", None, deadline, &mut stall);
    // Four restarts after 0.2 s each, then the fifth try has no limit and waits.
    let waited = tokio::time::timeout(Duration::from_secs(2), identify).await;
    assert!(waited.is_err());
    // The first try and four restarts.
    assert_eq!(5, connections.load(Ordering::Relaxed));
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
    // The fake tells the client that the players changed.
    heos.move_player("127.0.0.1");
    let title = || entity_state(&rig.events, EntityKind::MediaPlayer, "media_title");
    until("the HEOS title", 5, || title() == Some("A Song".into())).await;
}

#[actix::test]
async fn a_heos_command_without_an_answer_does_not_hold_up_the_receiver_commands() {
    let heos = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
    // The HEOS client waits longer than the driver.
    let patient = denon_marantz::heos::HeosConfig {
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
async fn heos_is_opened_again_when_the_receiver_connects_again() {
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
    // The `Avr` owns the HEOS connection: a new one opens HEOS again.
    until("HEOS connected again", 5, || {
        heos.count("player/get_players") > reads
    })
    .await;
}

#[actix::test]
async fn a_failed_first_heos_connect_is_tried_again() {
    // A closed port: HEOS refuses the first connects, then the fake opens on that port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    let config = denon_marantz::heos::HeosConfig {
        port,
        ..FakeHeos::start("127.0.0.1", HeosItem::song())
            .await
            .config()
    };
    TEST_HEOS.with(|slot| slot.set(Some(config)));
    let rig = rig(answers_all, QUICK).await;
    until("connected", 5, || {
        status_count(&rig.events, DeviceStatus::Connected) >= 1
    })
    .await;
    rig.telnet.send("PWON");
    rig.telnet.send("SINET");
    // The connect of the start has failed by then, also where a refused connect ends only at the
    // connect timeout (Windows).
    tokio::time::sleep(config.connect_timeout + Duration::from_millis(200)).await;
    let heos = FakeHeos::start_at(port, "127.0.0.1", HeosItem::song()).await;
    let title = || entity_state(&rig.events, EntityKind::MediaPlayer, "media_title");
    until("the HEOS title", 5, || title() == Some("A Song".into())).await;
    assert!(heos.count("player/get_players") >= 1);
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
async fn the_sound_mode_category_sensor_needs_the_receiver_to_list_it() {
    use crate::entities::sensor;
    use denon_marantz::{DeviceFeature, DeviceFeatures};
    let info = fake_info().await;
    let with = |features| DeviceConfig {
        info: Some(Info {
            features,
            ..info.clone()
        }),
        ..cfg()
    };
    let kind = SensorKind::SoundModeCategory;
    let listed = DeviceFeatures::default().with(DeviceFeature::SoundModeGenre);
    assert!(sensor::available(&with(Some(listed)), kind));
    assert!(!sensor::available(
        &with(Some(DeviceFeatures::default())),
        kind
    ));
    // A receiver without a feature list may have it.
    assert!(sensor::available(&with(None), kind));
    let http_only = DeviceConfig {
        use_telnet: false,
        ..with(Some(listed))
    };
    assert!(!sensor::available(&http_only, kind));
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
async fn changed_receiver_values_of_a_connect_before_a_new_configuration_are_not_used() {
    let mut info = fake_info().await;
    info.friendly_name = "Old name".into();
    let http = Fake::start(true).await;
    let stored = DeviceConfig {
        info: Some(Info {
            port: http.port(),
            ..info
        }),
        missing: denon_marantz::MissingSources {
            video_page: true,
            ..denon_marantz::MissingSources::default()
        },
        ..cfg()
    };
    let first = stored.clone();
    let rig = rig_on(http, move |_| first, answers_all, QUICK).await;
    // A new name does not connect again. The connect that runs has the old version.
    let renamed = DeviceConfig {
        name: "New name".into(),
        ..stored
    };
    rig.actor.send(UpdateConfig(renamed)).await.expect("update");
    until("connected", 5, || {
        status_count(&rig.events, DeviceStatus::Connected) >= 1
    })
    .await;
    // The start reads the signal after the check of the stored values.
    until("the signal read", 5, || rig.http.audyssey_reads() >= 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(1, status_count(&rig.events, DeviceStatus::Connected));
    let saved = configs(&rig.events);
    assert!(
        saved.iter().all(|c| c.missing.video_page
            && c.info
                .as_ref()
                .is_some_and(|i| i.friendly_name == "Old name")),
        "{saved:?}"
    );
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

/// A free port. It refuses connections until `open` passes them to `target`. The socket is bound
/// but does not listen: no other test can take the port meanwhile.
fn closed_port() -> (tokio::net::TcpSocket, u16) {
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket
        .bind((std::net::Ipv4Addr::LOCALHOST, 0).into())
        .expect("bind");
    let port = socket.local_addr().expect("address").port();
    (socket, port)
}

/// Pass the connections to the port of `socket` on to `target`.
fn open(socket: tokio::net::TcpSocket, target: u16) {
    let listener = socket.listen(16).expect("listen");
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
    let (socket, port) = closed_port();
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
        open(socket, fake.port());
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
    let (socket, port) = closed_port();
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
        open(socket, fake.port());
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
        ..cfg()
    };
    let rig = rig_with(cfg, answers_all, QUICK).await;
    let updated = || {
        rig.events.lock().expect("lock").iter().any(|event| {
            matches!(event, DeviceEvent::Config { cfg, .. }
                if cfg.info.as_ref().is_some_and(|i| i.quick_select_count == count))
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

const VIDEO_PAGE: &str = "/ajax/info/get_config?type=3";
const CHANNELS_PAGE: &str = "/ajax/info/get_config?type=2";

/// An actor in HTTP mode without the signal timer, connected to a fake receiver with the web
/// interface `web`. The HTTP poll runs every 100 ms and reads the signal pages.
async fn http_actor_with_web(
    cfg: DeviceConfig,
    web: &FakeWeb,
) -> (Addr<DenonActor>, Arc<Mutex<Vec<DeviceEvent>>>, Fake) {
    let http = Fake::start(true).await;
    let (http_port, web_port) = (http.port(), web.port());
    let setup: TestBuilder =
        Rc::new(move |_, builder| test_builder(http_port, 1, builder).web_interface_port(web_port));
    TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    TEST_DISCOVERY.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector(events.clone()).start();
    let cfg = DeviceConfig {
        use_telnet: false,
        signal_poll_secs: 0,
        ..cfg
    };
    let actor = DenonActor::new(cfg.id.as_str().into(), cfg, collector.recipient())
        .with_retry_delay(Duration::from_secs(60))
        .with_poll_interval(Duration::from_millis(100))
        .start();
    actor.send(Connect).await.expect("connect");
    (actor, events, http)
}

/// The configurations that the actor sent to the controller.
fn configs(events: &Mutex<Vec<DeviceEvent>>) -> Vec<DeviceConfig> {
    events
        .lock()
        .expect("lock")
        .iter()
        .filter_map(|event| match event {
            DeviceEvent::Config { cfg, .. } => Some(DeviceConfig::clone(cfg)),
            _ => None,
        })
        .collect()
}

fn pages_missing(cfg: &DeviceConfig) -> bool {
    cfg.missing.video_page && cfg.missing.channels_page
}

/// Connect until the actor saved the missing pages. Gives the saved configuration.
async fn learn_missing_pages(
    web: &FakeWeb,
) -> (
    Addr<DenonActor>,
    Arc<Mutex<Vec<DeviceEvent>>>,
    Fake,
    DeviceConfig,
) {
    let (actor, events, http) = http_actor_with_web(cfg(), web).await;
    until("the missing pages", 10, || {
        configs(&events).iter().any(pages_missing)
    })
    .await;
    let saved = configs(&events)
        .into_iter()
        .find(pages_missing)
        .expect("saved");
    (actor, events, http, saved)
}

#[actix::test]
async fn the_http_poll_reads_the_video_page_without_the_signal_timer() {
    // A 500 does not mark the page as missing, so each poll reads it again. Each read asks twice
    // (library retry), and the start reads it once.
    let web = FakeWeb::start(500).await;
    let (_actor, _events, _http) = http_actor_with_web(cfg(), &web).await;
    until("more reads of the video page", 10, || {
        web.requests(VIDEO_PAGE) > 6
    })
    .await;
}

#[actix::test]
async fn a_web_interface_that_does_not_answer_does_not_delay_the_http_poll() {
    // The pages time out after 500 ms; the state is read every 100 ms.
    let web = FakeWeb::start_slow(200, Duration::from_secs(10)).await;
    let (_actor, _events, http) = http_actor_with_web(cfg(), &web).await;
    until("the first page reads", 10, || web.requests(VIDEO_PAGE) > 0).await;
    let before = http.app_commands();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let reads = http.app_commands() - before;
    assert!(reads >= 6, "{reads} reads of the state in 1.5 s");
}

#[actix::test]
async fn missing_pages_are_saved_once_and_not_asked_after_a_reconnect() {
    let web = FakeWeb::start(404).await;
    let (actor, events, _http, saved) = learn_missing_pages(&web).await;
    // More polls and signal reads save nothing new.
    let before = configs(&events).len();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let later = configs(&events);
    assert!(
        later[before..].iter().all(|c| c.missing == saved.missing),
        "{:?}",
        later[before..]
            .iter()
            .map(|c| c.missing)
            .collect::<Vec<_>>()
    );
    actor.send(Disconnect).await.expect("disconnect");

    // With the stored values and missing sources, the pages are not asked again.
    let web = FakeWeb::start(404).await;
    let (_actor, events, _http) = http_actor_with_web(saved, &web).await;
    until("connected", 5, || {
        status_count(&events, DeviceStatus::Connected) == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        (0, 0),
        (web.requests(VIDEO_PAGE), web.requests(CHANNELS_PAGE))
    );
}

#[actix::test]
async fn new_receiver_values_clear_the_missing_sources() {
    // Missing sources without receiver values: the connect reads new values.
    let web = FakeWeb::start(404).await;
    let cfg = DeviceConfig {
        missing: denon_marantz::MissingSources {
            tone_control: true,
            video_page: true,
            ..denon_marantz::MissingSources::default()
        },
        ..cfg()
    };
    let (_actor, events, _http) = http_actor_with_web(cfg, &web).await;
    until("the receiver values", 10, || {
        configs(&events).iter().any(|c| c.info.is_some())
    })
    .await;
    let first = configs(&events)
        .into_iter()
        .find(|c| c.info.is_some())
        .expect("saved");
    assert!(!first.missing.tone_control, "{:?}", first.missing);
}

#[actix::test]
async fn a_new_setup_asks_the_missing_pages_again() {
    let web = FakeWeb::start(404).await;
    let (actor, events, _http, saved) = learn_missing_pages(&web).await;
    let connected = status_count(&events, DeviceStatus::Connected);
    let asked = web.requests(VIDEO_PAGE);
    // A new setup gives the configuration without missing sources.
    let setup = DeviceConfig {
        missing: denon_marantz::MissingSources::default(),
        ..saved
    };
    actor.send(UpdateConfig(setup)).await.expect("update");
    until("connected again", 5, || {
        status_count(&events, DeviceStatus::Connected) > connected
    })
    .await;
    until("the video page asked again", 5, || {
        web.requests(VIDEO_PAGE) > asked
    })
    .await;
}
