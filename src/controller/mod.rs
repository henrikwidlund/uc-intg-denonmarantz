// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Central controller: handles the Integration-API messages and the receiver actors.

mod devices;
mod handler;
mod messages;
#[cfg(test)]
pub(crate) mod test_util;
#[cfg(test)]
mod tests;

pub(crate) use handler::driver_version;
pub use messages::*;

use crate::config_store::{ConfigStore, DriverSettings};
use crate::configuration::{DEF_SETUP_TIMEOUT_SEC, ENV_SETUP_TIMEOUT, Settings};
use crate::controller::devices::Device;
use crate::controller::handler::{AbortDriverSetup, SetupState};
use crate::entities::EntityKind;
use crate::errors::ServiceError;
use actix::prelude::{Actor, Context, Recipient, SendError};
use actix::{AsyncContext, SpawnHandle};
use bytestring::ByteString;
use log::{debug, error, info, warn};
use rust_fsm::state_machine;
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;
use std::time::Duration;
use uc_api::intg::{DeviceState, IntegrationDriverUpdate};
use uc_api::ws::{EventCategory, WsMessage};

state_machine! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub operation_mode(RequireSetup)

    RequireSetup => {
        ConfigurationAvailable => Running,
        AbortSetup => RequireSetup,
        SetupDriverRequest => SetupFlow [SetupFlowTimer],
    },
    Running => {
        SetupDriverRequest => SetupFlow [SetupFlowTimer],
        R2Request => Running,
        AbortSetup => RequireSetup,
    },
    // The configured receivers stay usable during a setup flow.
    SetupFlow => {
        RequestUserInput => WaitSetupUserData,
        Successful => Running [CancelSetupFlowTimer],
        SetupError => SetupError [CancelSetupFlowTimer],
        AbortSetup => RequireSetup [CancelSetupFlowTimer],
        R2Request => SetupFlow,
    },
    WaitSetupUserData => {
        SetupUserData => SetupFlow,
        SetupError => SetupError [CancelSetupFlowTimer],
        AbortSetup => RequireSetup [CancelSetupFlowTimer],
        R2Request => WaitSetupUserData,
    },
    SetupError => {
        ConfigurationAvailable => Running,
        AbortSetup => RequireSetup,
        SetupDriverRequest => SetupFlow [SetupFlowTimer],
        SetupError => SetupError,
        R2Request => SetupError,
    }
}

/// One connected Remote.
#[allow(clippy::struct_excessive_bools)]
struct R2Session {
    recipient: Recipient<SendWsMessage>,
    /// Id of the last request message from the driver to the Remote.
    ws_id: u32,
    standby: bool,
    /// The Remote sent `disconnect`: it does not use the receivers until `connect`.
    paused: bool,
    /// Entities subscribed by id. Empty while `subscribe_all` is set.
    subscribed_entities: HashSet<String>,
    /// The Remote subscribed all entities, also the entities that come later.
    subscribe_all: bool,
    /// The Remote read the entity states with `get_entity_states` in this session.
    read_states: bool,
    /// The last `get_entity_states` answer had all entities because the Remote had no
    /// subscriptions, and the Remote did not subscribe since.
    read_all: bool,
    /// Entities that changed while the Remote was in standby, or after it read all states.
    missed: HashSet<String>,
    /// Flag if the session is in a reconfiguration setup flow.
    reconfiguring: Option<bool>,
}

impl R2Session {
    fn new(recipient: Recipient<SendWsMessage>) -> Self {
        Self {
            recipient,
            ws_id: 0,
            standby: false,
            paused: false,
            subscribed_entities: HashSet::default(),
            subscribe_all: false,
            read_states: false,
            read_all: false,
            missed: HashSet::default(),
            reconfiguring: None,
        }
    }

    /// The Remote uses the receivers: not in standby, and no `disconnect` since `connect`.
    const fn awake(&self) -> bool {
        !self.standby && !self.paused
    }

    /// True if the Remote subscribed an entity of the device `key`.
    fn uses(&self, key: &str) -> bool {
        self.subscribe_all
            || self
                .subscribed_entities
                .iter()
                .any(|id| EntityKind::parse(id).is_some_and(|(k, _)| k == key))
    }

    /// True if the Remote subscribed the entity.
    fn subscribed(&self, entity_id: &str) -> bool {
        self.subscribe_all || self.subscribed_entities.contains(entity_id)
    }

    /// True if the Remote subscribed at least one entity.
    fn has_subscriptions(&self) -> bool {
        self.subscribe_all || !self.subscribed_entities.is_empty()
    }

    const fn new_msg_id(&mut self) -> u32 {
        self.ws_id += 1;
        self.ws_id
    }
}

/// Central controller handling the Integration-API messages.
pub struct Controller {
    /// Connected Remotes.
    sessions: HashMap<Arc<str>, R2Session>,
    /// Driver state reported to the Remote. Receiver availability is shown with the entity states.
    device_state: DeviceState,
    drv_metadata: IntegrationDriverUpdate,
    /// State machine for the driver state: setup flow or running.
    machine: operation_mode::StateMachine,
    /// Driver setup timeout handle.
    setup_timeout: Option<SpawnHandle>,
    /// State of the active setup flow.
    setup: Option<SetupState>,
    /// Session that started the active setup flow.
    setup_owner: Option<Arc<str>>,
    /// Id of the setup flow. Changes on each start and end, so that old setup tasks are ignored.
    setup_flow: u32,
    /// A first setup or a reset: the receiver that the setup adds replaces the configured ones.
    /// Nothing is removed if the setup does not end with a receiver.
    setup_replace: bool,
    store: ConfigStore,
    driver_settings: DriverSettings,
    /// Configured receivers by device key.
    devices: HashMap<Arc<str>, Device>,
    /// Last sent attributes by entity id.
    entities: HashMap<String, serde_json::Map<String, serde_json::Value>>,
    /// Number of queued writes of `config.json`. A setup save compares it to find writes that
    /// were queued while it waited for its own.
    config_writes: u64,
}

impl Controller {
    pub fn new(
        settings: Settings,
        driver_settings: DriverSettings,
        drv_metadata: IntegrationDriverUpdate,
    ) -> Self {
        let store = ConfigStore::new(settings.config_home);
        Self {
            sessions: HashMap::default(),
            device_state: DeviceState::Connected,
            drv_metadata,
            machine: operation_mode::StateMachine::new(),
            setup_timeout: None,
            setup: None,
            setup_owner: None,
            setup_flow: 0,
            setup_replace: false,
            store,
            driver_settings,
            devices: HashMap::default(),
            entities: HashMap::default(),
            config_writes: 0,
        }
    }

    /// Send a WebSocket message to a Remote. Messages are dropped while the Remote is in standby.
    fn send_r2_msg(&self, message: impl Into<SendWsMessage>, ws_id: &str) {
        if let Some(session) = self.sessions.get(ws_id) {
            let message = message.into();
            if session.standby {
                if let SendWsMessage::Msg(m) = &message {
                    debug!(
                        "[{ws_id}] Remote is in standby, not sending message: {:?}",
                        m.msg
                    );
                }
                return;
            }
            match session.recipient.try_send(message) {
                Ok(()) => {}
                // The sender of the Remote stopped, for example after a disconnect.
                Err(SendError::Closed(_)) => {
                    debug!("[{ws_id}] Remote sender stopped, message dropped");
                }
                Err(SendError::Full(_)) => {
                    error!("[{ws_id}] Remote queue full, message dropped");
                }
            }
        } else {
            warn!("Attempting to send message but couldn't find session: {ws_id}");
        }
    }

    /// Send the answer to a request. It uses the same queue as the events, so a later event cannot
    /// come first. It is not dropped in standby or when the queue is full.
    fn send_r2_response(&self, message: impl Into<SendWsMessage>, ws_id: &str) {
        if let Some(session) = self.sessions.get(ws_id) {
            session.recipient.do_send(message.into());
        } else {
            warn!("Attempting to send response but couldn't find session: {ws_id}");
        }
    }

    /// Serialize a successful response and send it with [`Self::send_r2_response`].
    fn respond<T: Serialize>(
        &self,
        ws_id: &str,
        req_id: u32,
        msg: &str,
        msg_data: T,
    ) -> Result<(), ServiceError> {
        let text = response_text(req_id, msg, msg_data)?;
        self.send_r2_response(SendWsMessage::Text(text), ws_id);
        Ok(())
    }

    /// Send a `device_state` event to a Remote.
    fn send_device_state(&self, ws_id: &str) {
        debug!("[{ws_id}] sending device_state: {}", self.device_state);
        self.send_r2_msg(device_state_event(self.device_state), ws_id);
    }

    /// Perform a state machine transition for the given input.
    ///
    /// Returns an error if the transition is not allowed in the current state.
    fn sm_consume(
        &mut self,
        ws_id: &str,
        input: &operation_mode::Input,
        ctx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        let old_state = self.machine.state().clone();
        match self.machine.consume(input) {
            Ok(output) => {
                let state = self.machine.state();
                if *state != old_state {
                    info!("State machine transition: {old_state:?} -> {state:?}");
                }
                match output {
                    Some(operation_mode::Output::SetupFlowTimer) => {
                        if let Some(handle) = self.setup_timeout.take() {
                            ctx.cancel_future(handle);
                        }
                        if let Some(timeout) = setup_timeout() {
                            debug!("Starting SetupFlowTimer: {} sec", timeout.as_secs());
                            self.setup_timeout = Some(ctx.notify_later(
                                AbortDriverSetup {
                                    ws_id: ws_id.to_string(),
                                    timeout: true,
                                },
                                timeout,
                            ));
                        }
                    }
                    Some(operation_mode::Output::CancelSetupFlowTimer) => {
                        if let Some(handle) = self.setup_timeout.take() {
                            ctx.cancel_future(handle);
                        }
                    }
                    None => {}
                }
                Ok(())
            }
            Err(_) => Err(ServiceError::BadRequest(format!(
                "Transition {input:?} not allowed in state {old_state:?}"
            ))),
        }
    }
}

/// Successful response, the same as [`WsMessage::response`] gives. Borrows the data, so that it
/// is not copied to serialize it.
#[derive(Serialize)]
struct ResponseRef<'a, T> {
    kind: &'static str,
    req_id: u32,
    msg: &'a str,
    code: u16,
    msg_data: T,
}

/// Serialize a successful response.
fn response_text<T: Serialize>(
    req_id: u32,
    msg: &str,
    msg_data: T,
) -> Result<ByteString, ServiceError> {
    let response = ResponseRef {
        kind: "resp",
        req_id,
        msg,
        code: 200,
        msg_data,
    };
    serde_json::to_string(&response)
        .map(ByteString::from)
        .map_err(|e| ServiceError::InternalServerError(format!("Error serializing {msg}: {e}")))
}

fn device_state_event(state: DeviceState) -> WsMessage {
    WsMessage::event(
        "device_state",
        EventCategory::Device,
        json!({ "state": state }),
    )
}

/// Get the setup flow timeout. `None` if disabled with `UC_SETUP_TIMEOUT=0`.
fn setup_timeout() -> Option<Duration> {
    let timeout = env::var(ENV_SETUP_TIMEOUT).map_or(DEF_SETUP_TIMEOUT_SEC, |value| {
        value.trim().parse().unwrap_or_else(|_| {
            warn!("Invalid {ENV_SETUP_TIMEOUT} value '{value}', using default: {DEF_SETUP_TIMEOUT_SEC}");
            DEF_SETUP_TIMEOUT_SEC
        })
    });
    (timeout > 0).then(|| Duration::from_secs(timeout))
}

/// Messages that can wait for the controller. A request waits outside the queue while it is
/// full, so a later event could be handled first.
const MAILBOX_CAPACITY: usize = 1024;

impl Actor for Controller {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(MAILBOX_CAPACITY);
        let devices = self.store.load_devices();
        info!("{} configured receivers", devices.len());
        if !devices.is_empty() {
            let _ = self.sm_consume("", &operation_mode::Input::ConfigurationAvailable, ctx);
        }
        for cfg in devices {
            self.add_device(cfg, ctx);
        }
    }
}
