// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Switch entities: settings that are on or off.

use crate::config_store::DeviceConfig;
use crate::entities::{SwitchKind, View, availability};
use crate::errors::ServiceError;
use denon_marantz::proto::Trigger;
use denon_marantz::{Avr, State};
use serde_json::{Map, Value};
use std::str::FromStr;
use uc_api::intg::AvailableIntgEntity;
use uc_api::{SwitchCommand, SwitchDeviceClass, SwitchFeature};

pub fn definition(mut entity: AvailableIntgEntity) -> AvailableIntgEntity {
    entity.device_class = Some(SwitchDeviceClass::Switch.as_ref().into());
    entity.features = Some(
        [SwitchFeature::OnOff, SwitchFeature::Toggle]
            .iter()
            .map(|f| f.as_ref().to_string())
            .collect(),
    );
    entity
}

pub const fn name(kind: SwitchKind) -> &'static str {
    match kind {
        SwitchKind::ToneControl => "Tone Control",
        SwitchKind::DynamicEq => "Dynamic EQ",
        SwitchKind::Trigger1 => "Trigger 1",
        SwitchKind::Trigger2 => "Trigger 2",
        SwitchKind::Trigger3 => "Trigger 3",
        SwitchKind::BluetoothTransmitter => "Bluetooth Transmitter",
    }
}

/// The values come with telnet.
pub const fn available(cfg: &DeviceConfig, _kind: SwitchKind) -> bool {
    cfg.use_telnet
}

/// The setting is on or off. `None` if the receiver did not report it.
pub const fn value(state: &State, kind: SwitchKind) -> Option<bool> {
    let device = state.device();
    match kind {
        SwitchKind::ToneControl => device.tone_control,
        SwitchKind::DynamicEq => device.dynamic_eq,
        SwitchKind::Trigger1 => device.trigger(Trigger::One),
        SwitchKind::Trigger2 => device.trigger(Trigger::Two),
        SwitchKind::Trigger3 => device.trigger(Trigger::Three),
        SwitchKind::BluetoothTransmitter => device.bluetooth_transmitter,
    }
}

pub fn attributes(v: &View<'_>, kind: SwitchKind, out: &mut Map<String, Value>) {
    let state = match value(v.state, kind) {
        _ if availability(v) != "ON" => "UNAVAILABLE",
        Some(true) => "ON",
        Some(false) => "OFF",
        None => "UNKNOWN",
    };
    out.insert("state".into(), state.into());
}

/// The new state for a command: `on`, `off` or `toggle`.
fn target(cmd_id: &str, current: Option<bool>) -> Result<bool, ServiceError> {
    match SwitchCommand::from_str(cmd_id) {
        Ok(SwitchCommand::On) => Ok(true),
        Ok(SwitchCommand::Off) => Ok(false),
        Ok(SwitchCommand::Toggle) => Ok(!current.unwrap_or(false)),
        Err(_) => Err(ServiceError::NotSupported(cmd_id.into())),
    }
}

/// Run a switch command: `on`, `off` or `toggle`.
pub async fn command(avr: &Avr, kind: SwitchKind, cmd_id: &str) -> Result<(), ServiceError> {
    let current = value(&avr.state().borrow(), kind);
    let on = target(cmd_id, current)?;
    match kind {
        SwitchKind::ToneControl => avr.tone().set_enabled(on).await?,
        SwitchKind::DynamicEq => avr.audyssey().set_dynamic_eq(on).await?,
        SwitchKind::Trigger1 => avr.device().set_trigger(Trigger::One, on).await?,
        SwitchKind::Trigger2 => avr.device().set_trigger(Trigger::Two, on).await?,
        SwitchKind::Trigger3 => avr.device().set_trigger(Trigger::Three, on).await?,
        SwitchKind::BluetoothTransmitter => avr.device().set_bluetooth_transmitter(on).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_and_off_ignore_the_current_state() {
        for current in [None, Some(true), Some(false)] {
            assert!(target("on", current).expect("on"));
            assert!(!target("off", current).expect("off"));
        }
    }

    #[test]
    fn toggle_turns_the_state_around() {
        assert!(!target("toggle", Some(true)).expect("toggle"));
        assert!(target("toggle", Some(false)).expect("toggle"));
        // An unknown state counts as off.
        assert!(target("toggle", None).expect("toggle"));
    }

    #[test]
    fn other_commands_are_not_supported() {
        assert!(matches!(
            target("select_option", None),
            Err(ServiceError::NotSupported(_))
        ));
    }
}
