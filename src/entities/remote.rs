// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Remote entity: buttons, UI pages and commands. The commands, the pages and the button mapping
//! are the ones of the Python integration. Commands use the media player mapping.

use crate::config_store::{DeviceConfig, Feature};
use crate::entities::View;
use crate::entities::media_player::{self, Action};
use crate::entities::{is_off, simple_commands};
use crate::errors::ServiceError;
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;
use uc_api::intg::{
    AvailableIntgEntity, IntgRemoteCommand, IntgRemoteFeature, IntgRemoteOptionField,
};

/// Highest repeat count of a command.
const MAX_REPEAT: u64 = 20;
/// Longest pause between commands in milliseconds.
const MAX_DELAY_MS: u64 = 5_000;
/// Most commands in one sequence.
const MAX_SEQUENCE: usize = 100;

/// Button mapping and UI pages, from `resources/remote_ui.json`.
struct Ui {
    denon: Value,
    marantz: Value,
    buttons: Value,
}

static UI: LazyLock<Ui> = LazyLock::new(|| {
    let mut ui: Value = serde_json::from_str(include_str!("../../resources/remote_ui.json"))
        .unwrap_or_else(|_| json!({}));
    Ui {
        denon: ui["denon"].take(),
        marantz: ui["marantz"].take(),
        buttons: ui["buttons"].take(),
    }
});

pub fn definition(mut entity: AvailableIntgEntity, cfg: &DeviceConfig) -> AvailableIntgEntity {
    entity.features = Some(
        [
            IntgRemoteFeature::SendCmd,
            IntgRemoteFeature::OnOff,
            IntgRemoteFeature::Toggle,
        ]
        .iter()
        .map(|f| f.as_ref().to_string())
        .collect(),
    );
    let simple: Vec<&str> = media_player::BASE_COMMANDS
        .iter()
        .copied()
        .chain(simple_commands::supported(cfg))
        .collect();
    let mut options = Map::new();
    options.insert(
        IntgRemoteOptionField::SimpleCommands.as_ref().into(),
        json!(simple),
    );
    options.insert(
        IntgRemoteOptionField::ButtonMapping.as_ref().into(),
        UI.buttons.clone(),
    );
    let pages = if cfg.is_denon { &UI.denon } else { &UI.marantz };
    options.insert(
        IntgRemoteOptionField::UserInterface.as_ref().into(),
        json!({ "pages": pages_for(pages, cfg) }),
    );
    entity.options = Some(options);
    entity
}

/// The UI pages without the buttons of quick select slots and tuner commands that the receiver
/// does not have. A page without buttons is left out.
fn pages_for(pages: &Value, cfg: &DeviceConfig) -> Value {
    let tuner = cfg.offers(Feature::Tuner);
    let has = |item: &Value| {
        let cmd = item["command"]["cmd_id"].as_str();
        cmd.and_then(simple_commands::quick_select_slot)
            .is_none_or(|slot| cfg.has_quick_select(slot))
            && (tuner || !cmd.is_some_and(simple_commands::needs_tuner))
    };
    let Some(pages) = pages.as_array() else {
        return Value::Null;
    };
    if cfg.quick_select_count.is_none() && tuner {
        return Value::Array(pages.clone());
    }
    Value::Array(
        pages
            .iter()
            .filter_map(|page| {
                let items = page["items"].as_array()?;
                if items.iter().all(has) {
                    return Some(page.clone());
                }
                let kept: Vec<Value> = items.iter().filter(|i| has(i)).cloned().collect();
                if kept.is_empty() {
                    return None;
                }
                let mut page = page.clone();
                page["items"] = Value::Array(kept);
                Some(page)
            })
            .collect(),
    )
}

/// Entity state of the remote.
pub const fn state(v: &View<'_>) -> &'static str {
    if !v.connected {
        "UNAVAILABLE"
    } else if is_off(v) {
        "OFF"
    } else {
        "ON"
    }
}

/// Commands of the remote entity.
#[derive(Debug, Clone)]
pub struct RemoteRun {
    /// Actions to run in order. Empty for `stop_send`: the actor stops the running commands.
    pub actions: Vec<Action>,
    /// How often each action runs, before the next one starts.
    pub repeat: u64,
    /// Pause between commands.
    pub delay: Duration,
}

impl RemoteRun {
    /// How many commands the run sends.
    pub fn steps(&self) -> u64 {
        u64::try_from(self.actions.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(self.repeat)
    }
}

/// The command stops the running commands.
pub fn is_stop(cmd_id: &str) -> bool {
    matches!(
        IntgRemoteCommand::from_str(cmd_id),
        Ok(IntgRemoteCommand::StopSend)
    )
}

/// Map a remote entity command.
pub fn command(
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
) -> Result<RemoteRun, ServiceError> {
    let single = |action| RemoteRun {
        actions: vec![action],
        repeat: 1,
        delay: Duration::ZERO,
    };
    let Ok(cmd) = IntgRemoteCommand::from_str(cmd_id) else {
        // Other commands go to the receiver as they are.
        return media_player::command(cmd_id, params).map(single);
    };
    match cmd {
        IntgRemoteCommand::On => Ok(single(Action::On)),
        IntgRemoteCommand::Off => Ok(single(Action::Off)),
        IntgRemoteCommand::Toggle => Ok(single(Action::Toggle)),
        IntgRemoteCommand::SendCmd => {
            let command = media_player::param_str(params, "command")?;
            Ok(RemoteRun {
                actions: vec![map_command(command)?],
                repeat: repeat(params),
                delay: delay(params),
            })
        }
        IntgRemoteCommand::SendCmdSequence => {
            let sequence = params
                .and_then(|p| p.get("sequence"))
                .and_then(Value::as_array)
                .ok_or_else(|| ServiceError::BadRequest("Missing parameter: sequence".into()))?;
            if sequence.is_empty() || sequence.len() > MAX_SEQUENCE {
                return Err(ServiceError::BadRequest(format!(
                    "A sequence must have 1 to {MAX_SEQUENCE} commands"
                )));
            }
            let actions = sequence
                .iter()
                .map(|c| {
                    c.as_str()
                        .ok_or_else(|| {
                            ServiceError::BadRequest("Invalid command in sequence".into())
                        })
                        .and_then(map_command)
                })
                .collect::<Result<_, _>>()?;
            Ok(RemoteRun {
                actions,
                repeat: repeat(params),
                delay: delay(params),
            })
        }
        IntgRemoteCommand::StopSend => Ok(RemoteRun {
            actions: Vec::new(),
            repeat: 0,
            delay: Duration::ZERO,
        }),
    }
}

/// Map a command name. The names of the media player commands are in lower case.
fn map_command(command: &str) -> Result<Action, ServiceError> {
    if command.is_empty() {
        return Err(ServiceError::BadRequest(
            "The command parameter is missing".into(),
        ));
    }
    if command.starts_with("remote.") {
        return Err(ServiceError::BadRequest(format!(
            "Unsupported command: {command}"
        )));
    }
    media_player::command(command, None)
}

/// Repeat count. The Remote sends 4 for a held button: this is changed to 1 until the
/// Remote is fixed.
fn repeat(params: Option<&Map<String, Value>>) -> u64 {
    match param_u64(params, "repeat") {
        0 | 4 => 1,
        r => r.min(MAX_REPEAT),
    }
}

fn delay(params: Option<&Map<String, Value>>) -> Duration {
    Duration::from_millis(param_u64(params, "delay").min(MAX_DELAY_MS))
}

fn param_u64(params: Option<&Map<String, Value>>, name: &str) -> u64 {
    params
        .and_then(|p| p.get(name))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn the_ui_matches_the_python_integration() {
        assert_eq!(15, UI.denon.as_array().map_or(0, Vec::len));
        assert_eq!(15, UI.marantz.as_array().map_or(0, Vec::len));
        assert!(UI.buttons.as_array().is_some_and(|b| b.len() >= 15));
    }

    #[test]
    fn the_quick_select_page_has_the_slots_of_the_receiver() {
        let quick = |pages: &Value| -> Vec<String> {
            pages
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|p| p["items"].as_array().into_iter().flatten())
                .filter_map(|i| i["command"]["cmd_id"].as_str())
                .filter(|c| c.starts_with("QUICK_SELECT"))
                .map(str::to_owned)
                .collect()
        };
        let pages = |count| {
            let mut cfg = crate::entities::tests::cfg(true, true);
            cfg.quick_select_count = count;
            pages_for(&UI.denon, &cfg)
        };
        assert_eq!(6, quick(&pages(None)).len());
        assert_eq!(
            vec!["QUICK_SELECT_1", "QUICK_SELECT_2"],
            quick(&pages(Some(2)))
        );
        let all = UI.denon.as_array().map_or(0, Vec::len);
        assert_eq!(all, pages(Some(2)).as_array().map_or(0, Vec::len));
        // A page without buttons is left out.
        assert_eq!(all - 1, pages(Some(0)).as_array().map_or(0, Vec::len));
    }

    #[test]
    fn no_page_has_two_buttons_with_the_same_command() {
        for pages in [&UI.denon, &UI.marantz] {
            for page in pages.as_array().into_iter().flatten() {
                let mut seen = std::collections::HashSet::new();
                for item in page["items"].as_array().into_iter().flatten() {
                    if let Some(cmd) = item["command"]["cmd_id"].as_str() {
                        assert!(seen.insert(cmd), "{}: {cmd}", page["page_id"]);
                    }
                }
            }
        }
    }

    #[test]
    fn send_cmd() {
        let p = params(json!({ "command": "cursor_up", "repeat": 3, "delay": 100 }));
        let run = command("send_cmd", Some(&p)).expect("ok");
        assert!(matches!(run.actions.as_slice(), [Action::CursorUp]));
        assert_eq!(3, run.repeat);
        assert_eq!(Duration::from_millis(100), run.delay);
    }

    #[test]
    fn simple_and_raw_commands() {
        let p = params(json!({ "command": "OUTPUT_1" }));
        let run = command("send_cmd", Some(&p)).expect("ok");
        assert!(matches!(run.actions.as_slice(), [Action::Simple(_)]));
        let p = params(json!({ "command": "PWON" }));
        let run = command("send_cmd", Some(&p)).expect("ok");
        assert!(matches!(run.actions.as_slice(), [Action::Raw(_)]));
    }

    #[test]
    fn repeat_quirk() {
        for (value, expected) in [(0, 1), (1, 1), (4, 1), (5, 5), (50, MAX_REPEAT)] {
            let p = params(json!({ "command": "back", "repeat": value }));
            assert_eq!(
                expected,
                command("send_cmd", Some(&p)).expect("ok").repeat,
                "repeat {value}"
            );
        }
    }

    #[test]
    fn delay_is_limited() {
        let p = params(json!({ "sequence": ["back"], "delay": 60_000 }));
        let run = command("send_cmd_sequence", Some(&p)).expect("ok");
        assert_eq!(Duration::from_millis(MAX_DELAY_MS), run.delay);
    }

    #[test]
    fn sequence() {
        let p = params(json!({ "sequence": ["back", "cursor_down"] }));
        let run = command("send_cmd_sequence", Some(&p)).expect("ok");
        assert!(matches!(
            run.actions.as_slice(),
            [Action::Back, Action::CursorDown]
        ));
    }

    #[test]
    fn a_sequence_has_1_to_100_commands() {
        let sequence = |n: usize| params(json!({ "sequence": vec!["back"; n] }));
        assert!(command("send_cmd_sequence", Some(&sequence(0))).is_err());
        assert!(command("send_cmd_sequence", Some(&sequence(MAX_SEQUENCE))).is_ok());
        assert!(command("send_cmd_sequence", Some(&sequence(MAX_SEQUENCE + 1))).is_err());
    }

    #[test]
    fn stop_send_stops_and_sends_nothing() {
        assert!(is_stop("stop_send"));
        assert!(!is_stop("send_cmd"));
        assert_eq!(0, command("stop_send", None).expect("ok").steps());
    }

    #[test]
    fn rejects_remote_prefix_and_empty_commands() {
        let p = params(json!({ "command": "remote.on" }));
        assert!(command("send_cmd", Some(&p)).is_err());
        let p = params(json!({ "command": "" }));
        assert!(command("send_cmd", Some(&p)).is_err());
    }
}
