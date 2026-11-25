// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Test helpers: a controller with a new configuration folder, and Remotes that keep the
//! messages they get.

use crate::config_store::DriverSettings;
use crate::configuration::{HeartbeatSettings, Settings};
use crate::controller::{Controller, NewR2Session, R2RequestMsg, SendWsMessage};
use crate::device::DenonActor;
use crate::errors::ServiceError;
use actix::{Actor, Addr, Context, Handler, Message};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uc_api::intg::ws::R2Request;
use uc_api::ws::WsMessage;

/// Keeps all messages sent to a Remote, as JSON.
struct Remote(Arc<Mutex<Vec<Value>>>);

impl Actor for Remote {
    type Context = Context<Self>;
}

impl Handler<SendWsMessage> for Remote {
    type Result = ();
    fn handle(&mut self, msg: SendWsMessage, _: &mut Context<Self>) {
        let value = match msg {
            SendWsMessage::Msg(m) => serde_json::to_value(m).expect("json"),
            SendWsMessage::Text(t) => serde_json::from_str(&t).expect("json"),
        };
        self.0.lock().expect("lock").push(value);
    }
}

/// Does nothing. The answer comes after all messages that were queued before it.
#[derive(Message)]
#[rtype(result = "()")]
struct Probe;

impl Handler<Probe> for Remote {
    type Result = ();
    fn handle(&mut self, _: Probe, _: &mut Context<Self>) {}
}

impl Handler<Probe> for Controller {
    type Result = ();
    fn handle(&mut self, _: Probe, _: &mut Context<Self>) {}
}

/// The actor of the receiver with this device key.
#[derive(Message)]
#[rtype(result = "Option<Addr<DenonActor>>")]
pub(crate) struct DeviceAddr(pub &'static str);

impl Handler<DeviceAddr> for Controller {
    type Result = Option<Addr<DenonActor>>;
    fn handle(&mut self, msg: DeviceAddr, _: &mut Context<Self>) -> Self::Result {
        self.devices.get(msg.0).map(|d| d.addr.clone())
    }
}

/// The `Arc` key that the controller holds for this device key. Device events must use the same
/// `Arc`, or they are dropped.
#[derive(Message)]
#[rtype(result = "Option<Arc<str>>")]
pub(crate) struct DeviceKey(pub &'static str);

impl Handler<DeviceKey> for Controller {
    type Result = Option<Arc<str>>;
    fn handle(&mut self, msg: DeviceKey, _: &mut Context<Self>) -> Self::Result {
        self.devices
            .get_key_value(msg.0)
            .map(|(k, _)| Arc::clone(k))
    }
}

/// The number of entity ids in the subscription list of a Remote. `subscribe_all` is not counted.
#[derive(Message)]
#[rtype(result = "usize")]
pub(crate) struct Subscribed(pub &'static str);

impl Handler<Subscribed> for Controller {
    type Result = usize;
    fn handle(&mut self, msg: Subscribed, _: &mut Context<Self>) -> usize {
        self.sessions
            .get(msg.0)
            .map_or(0, |s| s.subscribed_entities.len())
    }
}

/// True if the Remote subscribed all entities, also the entities that come later.
#[derive(Message)]
#[rtype(result = "bool")]
pub(crate) struct SubscribesAll(pub &'static str);

impl Handler<SubscribesAll> for Controller {
    type Result = bool;
    fn handle(&mut self, msg: SubscribesAll, _: &mut Context<Self>) -> bool {
        self.sessions.get(msg.0).is_some_and(|s| s.subscribe_all)
    }
}

/// Answers when the store has done all queued file writes.
#[derive(Message)]
#[rtype(result = "()")]
pub(crate) struct FlushStore;

impl Handler<FlushStore> for Controller {
    type Result = ();
    fn handle(&mut self, _: FlushStore, _: &mut Context<Self>) {
        self.store.flush();
    }
}

/// Wait until the actor has stopped, then until the store has done all queued file writes.
pub(crate) async fn wait_stopped(controller: &Addr<Controller>, actor: &Addr<DenonActor>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while actor.connected() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("actor stopped");
    controller.send(FlushStore).await.expect("send");
}

/// A connected test Remote. Keeps every message it gets.
pub(crate) struct TestRemote {
    id: Arc<str>,
    messages: Arc<Mutex<Vec<Value>>>,
    addr: Addr<Remote>,
}

impl TestRemote {
    /// The `msg_data` of the messages with this `msg` name, in the order they came.
    pub(crate) fn filter(&self, msg: &str) -> Vec<Value> {
        self.messages
            .lock()
            .expect("lock")
            .iter()
            .filter(|m| m["msg"] == msg)
            .map(|m| m["msg_data"].clone())
            .collect()
    }

    /// The `msg_data` of the last message with this `msg` name. `Null` if there is none.
    pub(crate) fn last(&self, msg: &str) -> Value {
        self.messages
            .lock()
            .expect("lock")
            .iter()
            .rev()
            .find(|m| m["msg"] == msg)
            .map_or(Value::Null, |m| m["msg_data"].clone())
    }

    /// The index of the first message for which `f` is true.
    pub(crate) fn position(&self, f: impl Fn(&Value) -> bool) -> Option<usize> {
        self.messages.lock().expect("lock").iter().position(f)
    }

    /// Forget the messages received so far.
    pub(crate) fn clear(&self) {
        self.messages.lock().expect("lock").clear();
    }
}

/// The configuration folder of a test.
pub(crate) fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "uc-intg-denonmarantz-{name}-{}",
        std::process::id()
    ))
}

/// A controller with a new configuration folder, see [`test_dir`]. `config` is written to
/// `config.json`.
pub(crate) fn controller(name: &str, config: Option<&str>) -> Addr<Controller> {
    let dir = test_dir(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    if let Some(config) = config {
        std::fs::write(dir.join("config.json"), config).expect("config");
    }
    let settings = Settings {
        interface: "127.0.0.1".into(),
        http_port: 0,
        config_home: dir,
        heartbeat: HeartbeatSettings::default(),
    };
    let metadata = serde_json::from_value(json!({})).expect("metadata");
    Controller::new(settings, DriverSettings::default(), metadata).start()
}

/// Read `config.json` of the test `name`.
pub(crate) fn config_file(name: &str) -> Value {
    let text = std::fs::read_to_string(test_dir(name).join("config.json")).expect("config.json");
    serde_json::from_str(&text).expect("json")
}

/// A receiver at the loopback address, see [`use_closed_port`].
pub(crate) const LOOPBACK_CONFIG: &str = r#"[{"id": "LOOP0000001", "name": "AVR", "address": "127.0.0.1", "use_telnet": true, "is_denon": true}]"#;

/// The device key of [`LOOPBACK_CONFIG`].
pub(crate) const LOOPBACK_KEY: &str = "LOOP0000001";

/// The receivers and the setup on this thread connect to port 1 of the loopback address for HTTP
/// and telnet, whatever the configured address. Nothing listens there, so a connect fails at
/// once. The search finds nothing.
pub(crate) fn use_closed_port() {
    let setup: crate::device::discovery::TestBuilder = std::rc::Rc::new(|_, _| {
        crate::test_support::test_builder(1, 1, denon_marantz::Avr::builder("127.0.0.1"))
    });
    crate::device::discovery::TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
    crate::device::discovery::TEST_DISCOVERY.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
}

/// Connect a Remote with the session id `id`.
pub(crate) async fn connect(controller: &Addr<Controller>, id: &str) -> TestRemote {
    let messages = Arc::default();
    let addr = Remote(Arc::clone(&messages)).start();
    controller
        .send(NewR2Session {
            addr: addr.clone().recipient(),
            id: id.into(),
        })
        .await
        .expect("send");
    TestRemote {
        id: id.into(),
        messages,
        addr,
    }
}

/// Wait until the controller handled all queued messages, and then until the Remotes handled
/// all messages from the controller. Timers and spawned tasks are not waited for.
pub(crate) async fn settle(controller: &Addr<Controller>, remotes: &[&TestRemote]) {
    controller.send(Probe).await.expect("send");
    for remote in remotes {
        remote.addr.send(Probe).await.expect("send");
    }
}

/// Send a request of the Remote `ws_id` and give its response. `None` if the controller sent
/// the response to the Remote.
pub(crate) async fn request(
    controller: &Addr<Controller>,
    ws_id: &str,
    request: R2Request,
    msg_data: Option<Value>,
) -> Result<Option<WsMessage>, ServiceError> {
    let msg = R2RequestMsg {
        ws_id: ws_id.into(),
        req_id: 1,
        request,
        msg_data,
    };
    controller.send(msg).await.expect("send")
}

/// The entity ids in the `get_entity_states` answer that `remote` gets.
pub(crate) async fn entity_states(
    controller: &Addr<Controller>,
    remote: &TestRemote,
) -> Vec<String> {
    let ws_id = remote.id.as_ref();
    let response = request(controller, ws_id, R2Request::GetEntityStates, None)
        .await
        .expect("ok");
    assert!(response.is_none(), "the controller sends the answer");
    settle(controller, &[remote]).await;
    remote
        .last("entity_states")
        .as_array()
        .expect("list")
        .iter()
        .filter_map(|e| e["entity_id"].as_str().map(str::to_owned))
        .collect()
}
