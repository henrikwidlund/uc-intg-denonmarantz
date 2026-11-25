// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Receivers: one actor per receiver, discovery and commands.

mod actor;
#[cfg(test)]
mod actor_tests;
#[cfg(test)]
mod command_tests;
mod commands;
pub mod discovery;
pub mod heos;

pub use actor::DenonActor;
#[cfg(test)]
pub use actor::Wants;

use crate::config_store::DeviceConfig;
use crate::entities::EntityKind;
use crate::errors::ServiceError;
use actix::Message;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// An entity command is answered at most this long after it came. The core waits 10 s for the
/// answer.
pub const COMMAND_LIMIT: Duration = Duration::from_secs(9);

/// Connection status of a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    Disconnected,
    Connecting,
    Connected,
    /// The receiver cannot be used until the next connect request, for example it is not a
    /// receiver of this driver.
    Error,
}

/// Event from a device actor to the controller.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub enum DeviceEvent {
    /// Attributes that might have changed, per entity.
    Changes {
        key: Arc<str>,
        changes: Vec<(EntityKind, Map<String, Value>)>,
    },
    /// The configuration changed and must be saved. `version` counts the `UpdateConfig`
    /// messages that the actor got before it sent this.
    Config {
        key: Arc<str>,
        version: u64,
        cfg: Box<DeviceConfig>,
    },
    Status {
        key: Arc<str>,
        status: DeviceStatus,
    },
}

/// Connect, and stay connected.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Connect;

/// Disconnect, and stay disconnected.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Disconnect;

/// Disconnect and stop the actor.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct Stop;

/// New configuration of the device.
#[derive(Debug, Message)]
#[rtype(result = "()")]
pub struct UpdateConfig(pub DeviceConfig);

/// Entity command.
#[derive(Debug, Clone, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct Command {
    pub kind: EntityKind,
    pub cmd_id: String,
    pub params: Option<Map<String, Value>>,
    /// The answer comes at the latest at this time.
    pub deadline: Instant,
}

impl Command {
    /// A command that came now: the deadline is [`COMMAND_LIMIT`] from now.
    pub fn new(kind: EntityKind, cmd_id: String, params: Option<Map<String, Value>>) -> Self {
        Self {
            kind,
            cmd_id,
            params,
            deadline: Instant::now() + COMMAND_LIMIT,
        }
    }
}
