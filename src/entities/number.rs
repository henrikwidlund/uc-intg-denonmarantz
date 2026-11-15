// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Number entities: the bass and treble levels of the main zone.

use crate::config_store::DeviceConfig;
use crate::entities::{NumberKind, View, availability};
use crate::errors::ServiceError;
use denon_marantz::{Avr, State, Zone};
#[cfg(test)]
use serde_json::json;
use serde_json::{Map, Value};
use std::str::FromStr;
use uc_api::NumberCommand;
use uc_api::intg::AvailableIntgEntity;

/// Range of the main zone in dB.
const MIN: i8 = -6;
const MAX: i8 = 6;

pub fn definition(mut entity: AvailableIntgEntity, _kind: NumberKind) -> AvailableIntgEntity {
    let mut options = Map::new();
    options.insert("min".into(), MIN.into());
    options.insert("max".into(), MAX.into());
    options.insert("step".into(), 1.into());
    options.insert("decimals".into(), 0.into());
    options.insert("unit".into(), "dB".into());
    entity.options = Some(options);
    entity
}

pub const fn name(kind: NumberKind) -> &'static str {
    match kind {
        NumberKind::Bass => "Bass",
        NumberKind::Treble => "Treble",
    }
}

/// The values come with telnet, or with `AppCommand` over HTTP if the receiver lists the query.
pub const fn available(cfg: &DeviceConfig, _kind: NumberKind) -> bool {
    cfg.use_telnet || cfg.reads_tone_control()
}

/// The level in dB. `None` if the receiver did not report it.
pub fn value(state: &State, kind: NumberKind) -> Option<i8> {
    let zone = state.zone(Zone::Main);
    match kind {
        NumberKind::Bass => zone.bass,
        NumberKind::Treble => zone.treble,
    }
    .map(|level| level.db())
}

pub fn attributes(v: &View<'_>, kind: NumberKind, out: &mut Map<String, Value>) {
    out.insert("state".into(), availability(v).into());
    let value = value(v.state, kind).map_or(Value::Null, Value::from);
    out.insert("value".into(), value);
}

/// The new level in dB for a command: `set_value`, `increment` or `decrement`. A level outside the
/// range is moved into it.
fn target(
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
    current: Option<i8>,
) -> Result<i8, ServiceError> {
    let step = params
        .and_then(|p| p.get("step"))
        .and_then(Value::as_f64)
        .map_or(1, |s| s.round().max(1.0) as i8);
    match NumberCommand::from_str(cmd_id) {
        Ok(NumberCommand::SetValue) => {
            let value = params
                .and_then(|p| p.get("value"))
                .and_then(Value::as_f64)
                .ok_or_else(|| ServiceError::BadRequest("Missing parameter: value".into()))?;
            Ok(value.round().clamp(f64::from(MIN), f64::from(MAX)) as i8)
        }
        Ok(NumberCommand::Increment) => Ok(current.unwrap_or(0).saturating_add(step).min(MAX)),
        Ok(NumberCommand::Decrement) => Ok(current.unwrap_or(0).saturating_sub(step).max(MIN)),
        Err(_) => Err(ServiceError::NotSupported(cmd_id.into())),
    }
}

/// Run a number command: `set_value`, `increment` or `decrement`.
pub async fn command(
    avr: &Avr,
    kind: NumberKind,
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
) -> Result<(), ServiceError> {
    let current = value(&avr.state().borrow(), kind);
    let db = target(cmd_id, params, current)?;
    match kind {
        NumberKind::Bass => avr.tone().set_bass_db(db).await?,
        NumberKind::Treble => avr.tone().set_treble_db(db).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_has_the_range_of_the_main_zone() {
        let entity = AvailableIntgEntity {
            entity_id: String::new(),
            device_id: None,
            entity_type: uc_api::EntityType::Number,
            device_class: None,
            name: std::collections::HashMap::default(),
            icon: None,
            description: None,
            features: None,
            area: None,
            options: None,
            attributes: None,
        };
        let options = definition(entity, NumberKind::Bass)
            .options
            .expect("options");
        assert_eq!(serde_json::json!(-6), options["min"]);
        assert_eq!(serde_json::json!(6), options["max"]);
    }

    fn params(value: &Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    #[test]
    fn set_value_rounds_and_stays_in_the_range() {
        let set = |v: f64| target("set_value", Some(&params(&json!({ "value": v }))), Some(0));
        assert_eq!(3, set(3.0).expect("3"));
        assert_eq!(2, set(2.4).expect("2.4"));
        assert_eq!(6, set(10.0).expect("10"));
        assert_eq!(-6, set(-9.0).expect("-9"));
        assert!(matches!(
            target("set_value", None, Some(0)),
            Err(ServiceError::BadRequest(_))
        ));
    }

    #[test]
    fn increment_and_decrement_stop_at_the_limits() {
        assert_eq!(1, target("increment", None, Some(0)).expect("up"));
        assert_eq!(6, target("increment", None, Some(6)).expect("up"));
        assert_eq!(-1, target("decrement", None, Some(0)).expect("down"));
        assert_eq!(-6, target("decrement", None, Some(-6)).expect("down"));
        // An unknown level counts as 0 dB.
        assert_eq!(1, target("increment", None, None).expect("up"));
    }

    #[test]
    fn increment_takes_the_step_of_the_command() {
        let two = params(&json!({ "step": 2 }));
        assert_eq!(5, target("increment", Some(&two), Some(3)).expect("up"));
        assert_eq!(6, target("increment", Some(&two), Some(5)).expect("up"));
        assert_eq!(1, target("decrement", Some(&two), Some(3)).expect("down"));
    }

    #[test]
    fn other_commands_are_not_supported() {
        assert!(matches!(
            target("toggle", None, Some(0)),
            Err(ServiceError::NotSupported(_))
        ));
    }
}
