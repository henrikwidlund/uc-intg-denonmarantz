// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Receivers of the controller: entity states, subscriptions and commands.

use crate::config_store::{DeviceConfig, SaveWait};
use crate::controller::{Controller, R2RequestMsg, SendWsMessage};
use crate::device::{Command, Connect, DenonActor, DeviceEvent, DeviceStatus, Disconnect, Stop};
use crate::entities::{self, EntityKind, media_player};
use crate::errors::ServiceError;
use crate::util::{DeserializeMsgData, return_fut_err};
use actix::{Actor, Addr, AsyncContext, Context, Handler, ResponseFuture};
use bytestring::ByteString;
use log::{debug, info, warn};
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use uc_api::EntityType;
use uc_api::intg::{AvailableIntgEntity, DeviceState, EntityCommand};
use uc_api::ws::{EventCategory, WsMessage, WsResultMsgData};

/// Attribute that changes with each new position.
const POSITION_UPDATED_AT: &str = "media_position_updated_at";

/// `entity_change` data. Borrows the attributes, so that they are not copied to serialize them.
#[derive(Serialize)]
pub(super) struct EntityChangeRef<'a, A = &'a Map<String, Value>> {
    entity_type: EntityType,
    entity_id: &'a str,
    attributes: A,
}

/// The attributes of `keys`, read from the cache.
struct Picked<'a> {
    cache: &'a Map<String, Value>,
    keys: &'a [String],
}

impl Serialize for Picked<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.keys.len()))?;
        for key in self.keys {
            if let Some(value) = self.cache.get(key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// `entity_change` event, the same as [`WsMessage::event`] gives.
#[derive(Serialize)]
struct EntityChangeEvent<T> {
    kind: &'static str,
    msg: &'static str,
    cat: EventCategory,
    ts: jiff::Timestamp,
    msg_data: T,
}

/// Serialize an `entity_change` event once, to send it to each Remote.
fn entity_change_text<T: Serialize>(data: T) -> Option<ByteString> {
    let event = EntityChangeEvent {
        kind: "event",
        msg: "entity_change",
        cat: EventCategory::Entity,
        ts: jiff::Timestamp::now(),
        msg_data: data,
    };
    match serde_json::to_string(&event) {
        Ok(text) => Some(text.into()),
        Err(e) => {
            warn!("entity_change serialize error: {e}");
            None
        }
    }
}

/// One configured receiver.
pub(super) struct Device {
    pub cfg: DeviceConfig,
    pub addr: Addr<DenonActor>,
    pub status: DeviceStatus,
    /// An awake Remote uses the device, see [`Controller::update_connections`].
    pub wanted: bool,
}

impl Controller {
    /// Start the actor of a device.
    pub(super) fn add_device(&mut self, cfg: DeviceConfig, ctx: &Context<Self>) {
        let key: Arc<str> = cfg.key().into();
        let addr = DenonActor::new(key.clone(), cfg.clone(), ctx.address().recipient()).start();
        // Remove first: `insert` keeps the old key, and events are matched by the key instance.
        if let Some(old) = self.devices.remove(&*key) {
            old.addr.do_send(Stop);
        }
        self.devices.insert(
            key,
            Device {
                cfg,
                addr,
                status: DeviceStatus::Disconnected,
                wanted: false,
            },
        );
    }

    /// The configured receiver with this serial number or MAC address.
    pub(super) fn find_device(
        &self,
        serial: Option<&str>,
        mac: Option<&str>,
    ) -> Option<&DeviceConfig> {
        self.devices
            .values()
            .map(|d| &d.cfg)
            .find(|cfg| cfg.same_receiver(serial, mac) == Some(true))
    }

    /// True if the key belongs to the running actor of the device.
    fn is_current(&self, key: &Arc<str>) -> bool {
        self.devices
            .get_key_value(&**key)
            .is_some_and(|(k, _)| Arc::ptr_eq(k, key))
    }

    /// Stop the actor of a device and forget its entities.
    pub(super) fn remove_device(&mut self, key: &str) -> Option<DeviceConfig> {
        let device = self.devices.remove(key)?;
        device.addr.do_send(Stop);
        self.entities
            .retain(|id, _| EntityKind::parse(id).is_none_or(|(k, _)| k != key));
        // The removed device might have had an error.
        self.update_device_state();
        Some(device.cfg)
    }

    /// The configuration of all devices, sorted by name.
    pub(super) fn sorted_configs(&self) -> Vec<DeviceConfig> {
        let mut devices: Vec<DeviceConfig> = self.devices.values().map(|d| d.cfg.clone()).collect();
        devices.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        devices
    }

    /// Save the configuration of all devices. The write runs on its own thread. The result says
    /// when it is done.
    pub(super) fn save_devices(&self) -> Result<SaveWait, ServiceError> {
        Ok(self.store.save_devices(&self.sorted_configs())?)
    }

    /// Save the configuration of all devices without waiting for the disk.
    pub(super) fn queue_devices(&self) -> Result<(), ServiceError> {
        self.store.queue_devices(&self.sorted_configs())?;
        Ok(())
    }

    /// The entity definitions of all receivers, only of `entity_type` if it is given.
    pub(super) fn available_entities(
        &self,
        entity_type: Option<EntityType>,
    ) -> Vec<AvailableIntgEntity> {
        self.devices
            .values()
            .flat_map(|d| {
                EntityKind::all(&d.cfg)
                    .filter(|k| entity_type.is_none_or(|t| k.entity_type() == t))
                    .map(|k| entities::definition(&d.cfg, k))
            })
            .collect()
    }

    /// The ids of all entities of all receivers.
    fn all_entity_ids(&self) -> Vec<String> {
        self.devices
            .iter()
            .flat_map(|(key, d)| EntityKind::all(&d.cfg).map(|k| k.entity_id(key)))
            .collect()
    }

    /// States of the subscribed entities, or of all entities without subscriptions.
    pub(super) fn entity_states(&mut self, ws_id: &str) -> Vec<EntityChangeRef<'_>> {
        if let Some(session) = self.sessions.get_mut(ws_id) {
            session.read_states = true;
            session.read_all = session.subscribed_entities.is_empty();
            if session.read_all {
                session.missed.clear();
            }
        }
        let subscribed = self.sessions.get(ws_id).map(|s| &s.subscribed_entities);
        let mut states: Vec<EntityChangeRef<'_>> = self
            .entities
            .iter()
            .filter(|(id, _)| subscribed.is_none_or(|s| s.is_empty() || s.contains(*id)))
            .filter_map(|(id, attrs)| entity_change(id, attrs))
            .collect();
        states.sort_by_key(|s| send_order(s.entity_id));
        states
    }

    /// Subscribe a Remote to entities, and send only the states that it does not have. One event
    /// per entity can fill the event queue of the Remote, and the Remote then closes the
    /// connection.
    ///
    /// No ids means all entities.
    pub(super) fn subscribe(&mut self, ws_id: &str, entity_ids: Vec<String>) {
        let entity_ids = if entity_ids.is_empty() {
            self.all_entity_ids()
        } else {
            entity_ids
        };
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return;
        };
        // At the start of a session the Remote reads the states with `get_entity_states` before
        // or after it subscribes. Before: it has all states, and gets only the changes since.
        // After: it gets nothing now. Entities added later get their state.
        let read_all = std::mem::take(&mut session.read_all);
        let mut new = Vec::new();
        let mut send = Vec::new();
        for id in entity_ids {
            if session.subscribed_entities.insert(id.clone()) {
                if session.read_states && (!read_all || session.missed.remove(&id)) {
                    send.push(id.clone());
                }
                new.push(id);
            }
        }
        if read_all {
            let subscribed = &session.subscribed_entities;
            session.missed.retain(|id| subscribed.contains(id));
        }
        send.sort_by_key(|id| send_order(id));
        for id in &send {
            if let Some(change) = self.entities.get(id).and_then(|a| entity_change(id, a)) {
                self.send_entity_change(ws_id, &change);
            }
        }
        if !new.is_empty() {
            self.update_connections(false);
        }
    }

    /// Send the cached states of the entities that changed while a Remote was in standby. This
    /// happens mostly if another Remote stayed awake. Sending all entities at each wake-up can fill
    /// the event queue of the Remote.
    pub(super) fn send_missed_states(&mut self, ws_id: &str) {
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return;
        };
        let mut missed: Vec<String> = std::mem::take(&mut session.missed).into_iter().collect();
        missed.sort_by_key(|id| send_order(id));
        for id in &missed {
            if self
                .sessions
                .get(ws_id)
                .is_some_and(|s| s.subscribed_entities.contains(id))
                && let Some(change) = self.entities.get(id).and_then(|a| entity_change(id, a))
            {
                self.send_entity_change(ws_id, &change);
            }
        }
    }

    /// No ids means all entities.
    pub(super) fn unsubscribe(&mut self, ws_id: &str, entity_ids: &[String]) {
        let Some(session) = self.sessions.get_mut(ws_id) else {
            return;
        };
        if entity_ids.is_empty() {
            session.subscribed_entities.clear();
        }
        for id in entity_ids {
            session.subscribed_entities.remove(id);
        }
        self.update_connections(false);
    }

    /// Connect the devices that an awake Remote uses, and disconnect the others. A device is used
    /// if the Remote subscribed one of its entities.
    ///
    /// With `force`, a used device is asked to connect even if it was used before. After
    /// wake-up, this starts a connect that waits for its next try at once.
    pub(super) fn update_connections(&mut self, force: bool) {
        let sessions = &self.sessions;
        for (key, device) in &mut self.devices {
            let wanted = sessions.values().any(|s| s.awake() && s.uses(key));
            if wanted {
                if force || !device.wanted {
                    device.addr.do_send(Connect);
                }
            } else if device.wanted || device.status != DeviceStatus::Disconnected {
                // A command can connect a device that is not used, see `entity_command`.
                debug!("[{}] not in use, disconnecting", device.cfg.name);
                device.addr.do_send(Disconnect);
            }
            device.wanted = wanted;
        }
    }

    /// Forget the cached states and the subscriptions of entities that a receiver does not have
    /// any more, for example after the connection mode changed.
    pub(super) fn purge_entities(&mut self) {
        let devices = &self.devices;
        let keep = |id: &str| {
            EntityKind::parse(id).is_some_and(|(key, kind)| {
                devices
                    .get(key)
                    .is_some_and(|d| EntityKind::all(&d.cfg).any(|k| k == kind))
            })
        };
        self.entities.retain(|id, _| keep(id));
        for session in self.sessions.values_mut() {
            session.subscribed_entities.retain(|id| keep(id));
        }
        self.update_connections(false);
    }

    pub(super) fn entity_command(
        &self,
        msg: R2RequestMsg,
        _resp_msg: &'static str,
    ) -> ResponseFuture<Result<Option<WsMessage>, ServiceError>> {
        let req_id = msg.req_id;
        let command: EntityCommand = match msg.deserialize() {
            Ok(c) => c,
            Err(e) => return_fut_err!(e.into()),
        };
        let addr = match self.device_for(&command.entity_id) {
            Ok(v) => v,
            Err(e) => return_fut_err!(e),
        };
        // The Remote subscribes the entities it uses. The command still runs: the receiver
        // connects for it.
        if let Some((key, _)) = EntityKind::parse(&command.entity_id)
            && !self.sessions.values().any(|s| s.uses(key))
        {
            warn!(
                "command for {}, but no Remote subscribed an entity of this receiver",
                command.entity_id
            );
        }
        let (addr, kind) = addr;
        if kind.entity_type() != command.entity_type {
            return_fut_err!(ServiceError::BadRequest(format!(
                "Invalid entity type for {}",
                command.entity_id
            )));
        }
        let request = addr.send(Command {
            kind,
            cmd_id: command.cmd_id,
            params: command.params,
        });
        Box::pin(async move {
            request.await.unwrap_or_else(|e| Err(e.into()))?;
            Ok(Some(WsMessage::response(
                req_id,
                "result",
                WsResultMsgData::new("OK", "Command sent"),
            )))
        })
    }

    fn device_for(&self, entity_id: &str) -> Result<(Addr<DenonActor>, EntityKind), ServiceError> {
        let (key, kind) = EntityKind::parse(entity_id)
            .ok_or_else(|| ServiceError::NotFound(format!("Unknown entity: {entity_id}")))?;
        let device = self
            .devices
            .get(key)
            .ok_or_else(|| ServiceError::NotFound(format!("Unknown entity: {entity_id}")))?;
        Ok((device.addr.clone(), kind))
    }

    fn send_entity_change(&self, ws_id: &str, change: &EntityChangeRef<'_>) {
        if let Some(text) = entity_change_text(change) {
            self.send_r2_msg(SendWsMessage::Text(text), ws_id);
        }
    }

    /// Store changed attributes and send them to the subscribed Remotes.
    fn apply_changes(&mut self, key: &str, updates: Vec<(EntityKind, Map<String, Value>)>) {
        // Late event of a removed device.
        if !self.devices.contains_key(key) {
            return;
        }
        let mut entity_id = String::new();
        for (kind, attrs) in updates {
            entity_id.clear();
            kind.write_entity_id(key, &mut entity_id);
            // A Remote that read all states and did not subscribe yet also needs the changes.
            let wanted = self
                .sessions
                .values()
                .any(|s| s.read_all || s.subscribed_entities.contains(&entity_id));
            let changed = if let Some(cache) = self.entities.get_mut(&entity_id) {
                diff(cache, attrs, wanted)
            } else {
                let mut attrs = attrs;
                stamp_first_position(&mut attrs);
                let changed = if wanted {
                    attrs.keys().cloned().collect()
                } else {
                    Vec::new()
                };
                self.entities.insert(entity_id.clone(), attrs);
                changed
            };
            if !changed.is_empty() {
                self.send_changes(kind, &entity_id, &changed);
            }
        }
    }

    /// Send `entity_change` with the attributes of `keys` to the Remotes that subscribed the
    /// entity. The message is serialized once.
    fn send_changes(&mut self, kind: EntityKind, entity_id: &str, keys: &[String]) {
        // A Remote in standby gets the state when it wakes up. A Remote that read all states and
        // did not subscribe yet gets it when it subscribes.
        for session in self.sessions.values_mut() {
            let subscribed = session.subscribed_entities.contains(entity_id);
            if ((session.standby && subscribed) || (session.read_all && !subscribed))
                && !session.missed.contains(entity_id)
            {
                session.missed.insert(entity_id.to_owned());
            }
        }
        let mut targets = self
            .sessions
            .iter()
            .filter(|(_, s)| !s.standby && s.subscribed_entities.contains(entity_id))
            .map(|(ws_id, _)| &**ws_id)
            .peekable();
        let Some(cache) = self.entities.get(entity_id) else {
            return;
        };
        if targets.peek().is_none() {
            return;
        }
        let Some(text) = entity_change_text(EntityChangeRef {
            entity_type: kind.entity_type(),
            entity_id,
            attributes: Picked { cache, keys },
        }) else {
            return;
        };
        for target in targets {
            self.send_r2_msg(SendWsMessage::Text(text.clone()), target);
        }
    }

    /// Driver state: error while one device has an error.
    fn update_device_state(&mut self) {
        let state = if self
            .devices
            .values()
            .any(|d| d.status == DeviceStatus::Error)
        {
            DeviceState::Error
        } else {
            DeviceState::Connected
        };
        if state != self.device_state {
            self.device_state = state;
            for ws_id in self.sessions.keys() {
                self.send_device_state(ws_id);
            }
        }
    }
}

/// Merge new attributes into the cache. Returns the names of the attributes that changed if
/// `collect` is set.
fn diff(cache: &mut Map<String, Value>, attrs: Map<String, Value>, collect: bool) -> Vec<String> {
    let mut changed = Vec::new();
    let mut updated_at = false;
    let mut new_position = false;
    for (name, value) in attrs {
        if name == POSITION_UPDATED_AT {
            updated_at = true;
            continue;
        }
        if cache.get(&name) != Some(&value) {
            // The Remote counts on from the time while playing, so a resume needs a new one.
            new_position |= matches!(name.as_str(), "media_position" | "media_duration" | "state");
            if collect {
                changed.push(name.clone());
            }
            cache.insert(name, value);
        }
    }
    // The time is only sent with a new position or state.
    if updated_at && new_position {
        if collect {
            changed.push(POSITION_UPDATED_AT.into());
        }
        cache.insert(
            POSITION_UPDATED_AT.into(),
            media_player::position_timestamp().into(),
        );
    }
    changed
}

/// The first attributes of an entity: a position gets its time, so that the Remote counts on
/// from the start.
fn stamp_first_position(attrs: &mut Map<String, Value>) {
    if attrs.contains_key(POSITION_UPDATED_AT) && attrs.contains_key("media_position") {
        attrs.insert(
            POSITION_UPDATED_AT.into(),
            media_player::position_timestamp().into(),
        );
    }
}

/// The media player and the remote are sent first: they are the most important on the Remote.
fn send_order(entity_id: &str) -> u8 {
    match EntityKind::parse(entity_id) {
        Some((_, EntityKind::MediaPlayer)) => 0,
        Some((_, EntityKind::Remote)) => 1,
        _ => 2,
    }
}

fn entity_change<'a>(
    entity_id: &'a str,
    attributes: &'a Map<String, Value>,
) -> Option<EntityChangeRef<'a>> {
    let (_, kind) = EntityKind::parse(entity_id)?;
    Some(EntityChangeRef {
        entity_type: kind.entity_type(),
        entity_id,
        attributes,
    })
}

impl Handler<DeviceEvent> for Controller {
    type Result = ();

    fn handle(&mut self, msg: DeviceEvent, _: &mut Self::Context) {
        if let DeviceEvent::Changes { key, .. }
        | DeviceEvent::Config { key, .. }
        | DeviceEvent::Status { key, .. } = &msg
            && !self.is_current(key)
        {
            // From a removed or replaced actor.
            return;
        }
        match msg {
            DeviceEvent::Changes { key, changes } => self.apply_changes(&key, changes),
            DeviceEvent::Config { key, cfg } => {
                let Some(device) = self.devices.get_mut(&*key) else {
                    return;
                };
                // The event can be older than the settings of the controller.
                let Some(new_kind) = device.cfg.take_found(*cfg) else {
                    return;
                };
                info!("[{}] saving what the receiver reported", device.cfg.name);
                if new_kind {
                    // The receiver can have fewer entities now.
                    self.purge_entities();
                }
                // The device actors share this thread: do not wait for the disk.
                if let Err(e) = self.queue_devices() {
                    warn!("Saving the configuration failed: {e}");
                }
            }
            DeviceEvent::Status { key, status } => {
                if let Some(device) = self.devices.get_mut(&*key) {
                    device.status = status;
                    self.update_device_state();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_store::DriverSettings;
    use crate::configuration::{HeartbeatSettings, Settings};
    use crate::controller::R2Session;
    use serde_json::json;
    use std::sync::Mutex;

    /// Collects the texts sent to a Remote.
    struct Remote(Arc<Mutex<Vec<String>>>);

    impl Actor for Remote {
        type Context = Context<Self>;
    }

    impl Handler<SendWsMessage> for Remote {
        type Result = ();
        fn handle(&mut self, msg: SendWsMessage, _: &mut Context<Self>) {
            if let SendWsMessage::Text(text) = msg
                && let Ok(mut texts) = self.0.lock()
            {
                texts.push(text.to_string());
            }
        }
    }

    fn sent(texts: &Mutex<Vec<String>>) -> Vec<String> {
        texts.lock().expect("lock").clone()
    }

    fn new_controller() -> Controller {
        let settings = Settings {
            interface: "127.0.0.1".into(),
            http_port: 0,
            config_home: std::env::temp_dir().join("uc-intg-denonmarantz-missed"),
            heartbeat: HeartbeatSettings::default(),
        };
        let metadata = serde_json::from_value(json!({})).expect("metadata");
        Controller::new(settings, DriverSettings::default(), metadata)
    }

    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    #[actix::test]
    async fn a_remote_that_read_all_states_gets_only_the_changes_when_it_subscribes() {
        let mut controller = new_controller();
        let ids = ["media_player.KEY", "remote.KEY", "sensor.mute.KEY"];
        for id in ids {
            controller
                .entities
                .insert(id.into(), map(json!({ "state": "ON" })));
        }
        let texts = Arc::default();
        let session = R2Session::new(Remote(Arc::clone(&texts)).start().recipient());
        controller.sessions.insert("a".into(), session);
        // The Remote reads the states before it subscribes, for example after a restart of the
        // driver or at the end of a setup.
        assert_eq!(3, controller.entity_states("a").len());
        let (_, kind) = EntityKind::parse(ids[0]).expect("id");
        controller
            .entities
            .insert(ids[0].into(), map(json!({ "state": "OFF" })));
        controller.send_changes(kind, ids[0], &["state".into()]);
        controller.subscribe("a", vec![ids[0].into(), ids[1].into()]);
        settle().await;
        let sent_now = sent(&texts);
        assert_eq!(1, sent_now.len());
        assert!(sent_now[0].contains(ids[0]), "{}", sent_now[0]);
        // An entity that is added later gets its state.
        controller.subscribe("a", vec![ids[2].into()]);
        settle().await;
        assert_eq!(2, sent(&texts).len());
    }

    #[actix::test]
    async fn the_media_player_and_the_remote_come_first() {
        let mut controller = new_controller();
        let ids = [
            "sensor.mute.KEY",
            "select.dimmer.KEY",
            "remote.KEY",
            "switch.tone_control.KEY",
            "media_player.KEY",
        ];
        for id in ids {
            controller
                .entities
                .insert(id.into(), map(json!({ "state": "ON" })));
        }
        let texts = Arc::default();
        let session = R2Session::new(Remote(Arc::clone(&texts)).start().recipient());
        controller.sessions.insert("a".into(), session);
        let first: Vec<&str> = controller
            .entity_states("a")
            .iter()
            .take(2)
            .map(|s| s.entity_id)
            .collect();
        assert_eq!(vec!["media_player.KEY", "remote.KEY"], first);
        // Entities that are added later get their state.
        controller.subscribe("a", vec!["number.bass.KEY".into()]);
        controller.subscribe("a", ids.map(str::to_owned).to_vec());
        settle().await;
        let sent_now = sent(&texts);
        assert_eq!(5, sent_now.len());
        assert!(sent_now[0].contains("media_player.KEY"), "{}", sent_now[0]);
        assert!(sent_now[1].contains("remote.KEY"), "{}", sent_now[1]);
    }

    #[actix::test]
    async fn a_remote_that_wakes_up_gets_only_the_changes_it_missed() {
        let mut controller = new_controller();
        let id = "media_player.KEY";
        let (_, kind) = EntityKind::parse(id).expect("id");
        controller
            .entities
            .insert(id.into(), map(json!({ "state": "ON" })));
        let (a, b) = (Arc::default(), Arc::default());
        for (ws_id, texts) in [("a", &a), ("b", &b)] {
            let mut session = R2Session::new(Remote(Arc::clone(texts)).start().recipient());
            session.subscribed_entities.insert(id.into());
            controller.sessions.insert(ws_id.into(), session);
        }
        let set_standby = |c: &mut Controller, standby| {
            c.sessions.get_mut("a").expect("session").standby = standby;
        };

        set_standby(&mut controller, true);
        controller
            .entities
            .insert(id.into(), map(json!({ "state": "OFF" })));
        controller.send_changes(kind, id, &["state".into()]);
        settle().await;
        assert_eq!(1, sent(&b).len());
        assert_eq!(0, sent(&a).len());
        set_standby(&mut controller, false);
        controller.send_missed_states("a");
        settle().await;
        let texts = sent(&a);
        assert_eq!(1, texts.len());
        assert!(texts[0].contains(r#""state":"OFF""#), "{}", texts[0]);
        // Nothing changed during this standby.
        set_standby(&mut controller, true);
        set_standby(&mut controller, false);
        controller.send_missed_states("a");
        settle().await;
        assert_eq!(1, sent(&a).len());
    }

    /// A device whose actor never runs: the messages to it stay in its queue.
    fn add_idle_device(controller: &mut Controller, key: &str) {
        let cfg: DeviceConfig =
            serde_json::from_value(json!({ "id": key, "name": key, "address": "192.0.2.1" }))
                .expect("config");
        let addr = Context::<DenonActor>::new().address();
        controller.devices.insert(
            key.into(),
            Device {
                cfg,
                addr,
                status: DeviceStatus::Disconnected,
                wanted: false,
            },
        );
    }

    fn add_session(controller: &mut Controller, ws_id: &str) {
        let session = R2Session::new(Remote(Arc::default()).start().recipient());
        controller.sessions.insert(ws_id.into(), session);
    }

    fn session<'a>(controller: &'a mut Controller, ws_id: &str) -> &'a mut R2Session {
        controller.sessions.get_mut(ws_id).expect("session")
    }

    fn wanted(controller: &Controller) -> Vec<&str> {
        let mut keys: Vec<&str> = controller
            .devices
            .iter()
            .filter(|(_, d)| d.wanted)
            .map(|(k, _)| &**k)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[actix::test]
    async fn only_the_receivers_with_subscribed_entities_are_connected() {
        let mut controller = new_controller();
        add_idle_device(&mut controller, "A");
        add_idle_device(&mut controller, "B");
        add_session(&mut controller, "a");
        // `connect` before `subscribe_events`, for example after a driver start.
        controller.update_connections(true);
        assert_eq!(Vec::<&str>::new(), wanted(&controller));
        controller.subscribe("a", vec!["media_player.A".into(), "remote.A".into()]);
        assert_eq!(vec!["A"], wanted(&controller));
        controller.unsubscribe("a", &["media_player.A".into()]);
        assert_eq!(vec!["A"], wanted(&controller));
        controller.unsubscribe("a", &["remote.A".into()]);
        assert_eq!(Vec::<&str>::new(), wanted(&controller));
        // No ids means all entities.
        controller.subscribe("a", vec!["media_player.B".into()]);
        controller.unsubscribe("a", &[]);
        assert_eq!(Vec::<&str>::new(), wanted(&controller));
    }

    #[actix::test]
    async fn a_receiver_stays_connected_while_an_awake_remote_uses_it() {
        let mut controller = new_controller();
        add_idle_device(&mut controller, "A");
        add_idle_device(&mut controller, "B");
        for ws_id in ["a", "b"] {
            add_session(&mut controller, ws_id);
            controller.subscribe(ws_id, vec!["media_player.A".into()]);
        }
        controller.subscribe("b", vec!["media_player.B".into()]);
        assert_eq!(vec!["A", "B"], wanted(&controller));
        session(&mut controller, "b").standby = true;
        controller.update_connections(false);
        assert_eq!(vec!["A"], wanted(&controller));
        // `disconnect` of the last awake Remote.
        session(&mut controller, "a").paused = true;
        controller.update_connections(false);
        assert_eq!(Vec::<&str>::new(), wanted(&controller));
        session(&mut controller, "b").standby = false;
        controller.update_connections(true);
        assert_eq!(vec!["A", "B"], wanted(&controller));
        // A Remote that ends takes its subscriptions with it.
        controller.sessions.remove("b");
        controller.update_connections(false);
        assert_eq!(Vec::<&str>::new(), wanted(&controller));
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn diff_keeps_only_changes() {
        let mut cache = map(json!({ "state": "ON", "media_position": 1 }));
        let changed = diff(
            &mut cache,
            map(json!({ "state": "ON", "media_title": "Song" })),
            true,
        );
        assert_eq!(vec!["media_title"], changed);
        assert_eq!(Some(&json!("Song")), cache.get("media_title"));
    }

    fn keys(m: &Map<String, Value>) -> Vec<&str> {
        let mut k: Vec<&str> = m.keys().map(String::as_str).collect();
        k.sort_unstable();
        k
    }

    #[test]
    fn the_position_time_is_sent_only_with_a_new_position_or_state() {
        let at =
            || map(json!({ "state": "PLAYING", "media_position": 10, POSITION_UPDATED_AT: null }));
        let mut cache = at();
        // Same position and state: nothing, also no new time.
        assert_eq!(Vec::<String>::new(), diff(&mut cache, at(), true));
        let moved =
            map(json!({ "state": "PLAYING", "media_position": 30, POSITION_UPDATED_AT: null }));
        let mut changed = diff(&mut cache, moved, true);
        changed.sort_unstable();
        assert_eq!(vec!["media_position", POSITION_UPDATED_AT], changed);
        assert!(cache[POSITION_UPDATED_AT].is_string());
        let paused =
            map(json!({ "state": "PAUSED", "media_position": 30, POSITION_UPDATED_AT: null }));
        let mut changed = diff(&mut cache, paused, true);
        changed.sort_unstable();
        assert_eq!(vec![POSITION_UPDATED_AT, "state"], changed);
        // Another attribute alone does not change the time.
        let titled = map(json!({ "media_title": "Song", POSITION_UPDATED_AT: null }));
        assert_eq!(vec!["media_title"], diff(&mut cache, titled, true));
    }

    #[test]
    fn the_first_position_gets_a_time() {
        let mut attrs = map(json!({ "media_position": 10, POSITION_UPDATED_AT: null }));
        stamp_first_position(&mut attrs);
        assert!(attrs[POSITION_UPDATED_AT].is_string());
        // An entity without a position gets no time.
        let mut other = map(json!({ "state": "ON" }));
        stamp_first_position(&mut other);
        assert_eq!(None, other.get(POSITION_UPDATED_AT));
    }

    #[test]
    fn diff_without_collect_updates_cache_only() {
        let mut cache = map(json!({ "media_position": 1 }));
        let changed = diff(&mut cache, map(json!({ "media_position": 2 })), false);
        assert_eq!(Vec::<String>::new(), changed);
        assert_eq!(vec!["media_position"], keys(&cache));
    }

    #[test]
    fn entity_change_text_has_only_the_keys() {
        let cache = map(json!({ "state": "ON", "media_title": "Song" }));
        let keys = ["media_title".to_string()];
        let text = entity_change_text(EntityChangeRef {
            entity_type: EntityType::MediaPlayer,
            entity_id: "media_player.x",
            attributes: Picked {
                cache: &cache,
                keys: &keys,
            },
        })
        .expect("text");
        let v: Value = serde_json::from_str(&text).expect("json");
        assert_eq!("event", v["kind"]);
        assert_eq!("entity_change", v["msg"]);
        assert_eq!("ENTITY", v["cat"]);
        assert!(v["ts"].is_string());
        assert_eq!(
            json!({ "entity_type": "media_player", "entity_id": "media_player.x", "attributes": { "media_title": "Song" } }),
            v["msg_data"]
        );
    }
}
