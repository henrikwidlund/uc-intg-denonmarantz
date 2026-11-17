// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Run entity commands on a receiver.

use crate::config_store::DeviceConfig;
use crate::device::heos;
use crate::entities::media_player::{self, Action};
use crate::entities::{
    EntityKind, SelectKind, View, is_off, number, remote, select, simple_commands, switch,
};
use crate::errors::ServiceError;
use denon_marantz::proto::Power;
use denon_marantz::{Avr, Command, RemoteKey, State, Zone};
use denon_marantz_heos::PlayState;
use serde_json::{Map, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// A volume or mute value from a command is used for the next toggle or step until the receiver
/// reports it.
const EXPECTED_MAX_AGE: Duration = Duration::from_secs(3);
/// Volume step of the receiver for volume up and down in dB.
const NATIVE_STEP: f32 = 0.5;
const MAX_DB: f32 = 18.0;
const MIN_DB: f32 = -80.0;

/// Values set by the last commands.
#[derive(Debug, Default)]
pub struct Expected {
    volume: Mutex<Option<(Instant, f32)>>,
    mute: Mutex<Option<(Instant, bool)>>,
}

fn fresh<T: Copy>(slot: &Mutex<Option<(Instant, T)>>) -> Option<T> {
    let guard = slot.lock().ok()?;
    guard
        .filter(|(at, _)| at.elapsed() < EXPECTED_MAX_AGE)
        .map(|(_, v)| v)
}

fn store<T>(slot: &Mutex<Option<(Instant, T)>>, value: T) {
    if let Ok(mut guard) = slot.lock() {
        *guard = Some((Instant::now(), value));
    }
}

impl Expected {
    /// Forget the values, for example after a new connect.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.volume.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.mute.lock() {
            *guard = None;
        }
    }
}

/// Run the steps of a remote command one after the other. The lock is taken for each step only,
/// so that other commands, for example the volume buttons, do not wait for a long run.
pub async fn run_steps(
    avr: &Avr,
    heos: Option<&heos::Link>,
    cfg: &DeviceConfig,
    expected: &Expected,
    run: &remote::RemoteRun,
    lock: &tokio::sync::Mutex<()>,
) -> Result<(), ServiceError> {
    let mut first = true;
    for action in &run.actions {
        for _ in 0..run.repeat {
            if !first && !run.delay.is_zero() {
                sleep(run.delay).await;
            }
            first = false;
            let _guard = lock.lock().await;
            action_run(avr, heos, cfg, expected, action.clone()).await?;
        }
    }
    Ok(())
}

/// Run an entity command.
pub async fn run(
    avr: &Avr,
    heos: Option<&heos::Link>,
    cfg: &DeviceConfig,
    expected: &Expected,
    kind: EntityKind,
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
) -> Result<(), ServiceError> {
    match kind {
        EntityKind::MediaPlayer => {
            let action = media_player::command(cmd_id, params)?;
            action_run(avr, heos, cfg, expected, action).await
        }
        EntityKind::Remote => {
            let run = remote::command(cmd_id, params)?;
            // Each command is repeated before the next one starts, as in the Python integration.
            let mut first = true;
            for action in &run.actions {
                for _ in 0..run.repeat {
                    if !first && !run.delay.is_zero() {
                        sleep(run.delay).await;
                    }
                    first = false;
                    action_run(avr, heos, cfg, expected, action.clone()).await?;
                }
            }
            Ok(())
        }
        EntityKind::Select(select_kind) => {
            let state = avr.snapshot();
            let view = command_view(&state, cfg);
            // The receiver ignores most settings while it is off.
            if is_off(&view) && select_kind != SelectKind::InputSource {
                return Ok(());
            }
            select_command(avr, &view, select_kind, cmd_id, params).await
        }
        EntityKind::Sensor(_) => Err(ServiceError::NotSupported(cmd_id.into())),
        EntityKind::Switch(switch_kind) => {
            // The receiver ignores most settings while it is off.
            if receiver_off(avr) {
                return Ok(());
            }
            switch::command(avr, switch_kind, cmd_id).await
        }
        EntityKind::Number(number_kind) => {
            if receiver_off(avr) {
                return Ok(());
            }
            number::command(avr, number_kind, cmd_id, params).await
        }
    }
}

/// The main zone is in standby.
fn receiver_off(avr: &Avr) -> bool {
    matches!(
        avr.state().borrow().zone(Zone::Main).power,
        Some(Power::Standby | Power::Off)
    )
}

async fn select_command(
    avr: &Avr,
    view: &View<'_>,
    kind: SelectKind,
    cmd_id: &str,
    params: Option<&Map<String, Value>>,
) -> Result<(), ServiceError> {
    let choices = select::choices(view, kind);
    let option = match cmd_id {
        "select_option" => option_param(params)?,
        "select_first" => choices.options.first().cloned().unwrap_or_default(),
        "select_last" => choices.options.last().cloned().unwrap_or_default(),
        "select_next" | "select_previous" => {
            let cycle = params
                .and_then(|p| p.get("cycle"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let forward = cmd_id == "select_next";
            step(&choices, forward, cycle)
                .ok_or_else(|| ServiceError::BadRequest("No option to select".into()))?
        }
        _ => return Err(ServiceError::NotSupported(cmd_id.into())),
    };
    select::select_option(avr, view, kind, &option).await
}

/// The option of a select command. The speaker preset has numbers.
fn option_param(params: Option<&Map<String, Value>>) -> Result<String, ServiceError> {
    match params.and_then(|p| p.get("option")) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(ServiceError::BadRequest("Missing parameter: option".into())),
    }
}

/// The option after or before the current one.
fn step(choices: &select::Choices, forward: bool, cycle: bool) -> Option<String> {
    let len = choices.options.len();
    if len == 0 {
        return None;
    }
    let position = choices
        .current
        .as_ref()
        .and_then(|c| choices.options.iter().position(|o| o == c));
    let next = match (position, forward) {
        (None, true) => 0,
        (None, false) => len - 1,
        (Some(i), true) if i + 1 < len => i + 1,
        (Some(_), true) => return cycle.then(|| choices.options[0].clone()),
        (Some(i), false) if i > 0 => i - 1,
        (Some(_), false) => return cycle.then(|| choices.options[len - 1].clone()),
    };
    Some(choices.options[next].clone())
}

async fn send(avr: &Avr, command: Command<'_>) -> Result<(), ServiceError> {
    // Telnet if it is connected, else HTTP.
    avr.send(Zone::Main, command).await?;
    Ok(())
}

/// Volume up or down by the configured step. The step of the receiver is used when it is the
/// same, other steps are set as a new volume.
async fn volume_step(
    avr: &Avr,
    cfg: &DeviceConfig,
    expected: &Expected,
    up: bool,
) -> Result<(), ServiceError> {
    let zone = avr.zone(Zone::Main)?;
    if (cfg.volume_step - NATIVE_STEP).abs() < f32::EPSILON {
        if up {
            zone.volume_up().await?;
        } else {
            zone.volume_down().await?;
        }
        return Ok(());
    }
    let state = avr.snapshot();
    let current = state.zone(Zone::Main).volume.map(denon_marantz::Volume::db);
    let base = fresh(&expected.volume)
        .or(current)
        .ok_or_else(|| ServiceError::ServiceUnavailable("The volume is not known yet".into()))?;
    let step = if up {
        cfg.volume_step
    } else {
        -cfg.volume_step
    };
    let db = (base + step).clamp(MIN_DB, MAX_DB);
    store(&expected.volume, db);
    zone.set_volume_db(db).await?;
    Ok(())
}

/// What the commands read of the state. They do not read the active sound mode or HEOS. A group is
/// a valid sound mode while the receiver gave no list.
const fn command_view<'a>(state: &'a State, cfg: &'a DeviceConfig) -> View<'a> {
    View {
        state,
        cfg,
        connected: true,
        artwork_url: None,
        paused: false,
        sound_mode: None,
        sound_groups: true,
        heos: None,
        position: None,
    }
}

/// Longest wait for HEOS to answer a command. The HEOS client sends one command at a time, so a
/// command can wait behind a slow one. A command that is still waiting then is not sent.
const HEOS_COMMAND_LIMIT: Duration = Duration::from_secs(5);

/// Play, pause, stop, next and previous: the actions that go to HEOS while it is used.
pub const fn is_transport(action: &Action) -> bool {
    matches!(
        action,
        Action::PlayPause | Action::Stop | Action::Next | Action::Previous
    )
}

/// The media player shows HEOS: transport actions go to HEOS.
pub fn heos_in_use(heos: &heos::Link, state: &State) -> bool {
    heos.with_player(|p| p.is_some_and(|p| media_player::uses_heos(state, p)))
}

/// Run a transport action on HEOS, for at most [`HEOS_COMMAND_LIMIT`].
pub async fn heos_run(heos: &heos::Link, action: &Action) -> Result<(), ServiceError> {
    let run = async {
        match action {
            Action::PlayPause => heos.play_pause().await,
            Action::Stop => heos.set_play_state(PlayState::Stop).await,
            Action::Next => heos.next().await,
            _ => heos.previous().await,
        }
    };
    tokio::time::timeout(HEOS_COMMAND_LIMIT, run)
        .await
        .map_err(|_| ServiceError::Timeout)?
}

/// Play, pause, stop, next and previous go to HEOS while the media player shows HEOS. `None` for
/// the other actions, and when HEOS is not used.
async fn heos_transport(
    heos: Option<&heos::Link>,
    state: &State,
    action: &Action,
) -> Option<Result<(), ServiceError>> {
    if !is_transport(action) {
        return None;
    }
    let heos = heos.filter(|link| heos_in_use(link, state))?;
    Some(heos_run(heos, action).await)
}

async fn action_run(
    avr: &Avr,
    heos: Option<&heos::Link>,
    cfg: &DeviceConfig,
    expected: &Expected,
    action: Action,
) -> Result<(), ServiceError> {
    let zone = avr.zone(Zone::Main)?;
    let state = avr.snapshot();
    let main = state.zone(Zone::Main);
    if let Some(result) = heos_transport(heos, &state, &action).await {
        return result;
    }
    match action {
        Action::On => zone.power_on().await?,
        Action::Off => zone.power_standby().await?,
        Action::Toggle => {
            if main.power == Some(Power::On) {
                zone.power_standby().await?;
            } else {
                zone.power_on().await?;
            }
        }
        Action::PlayPause => {
            if state.device().now_playing.is_playing() == Some(true) {
                send(avr, Command::Pause).await?;
            } else {
                send(avr, Command::Play).await?;
            }
        }
        // Without HEOS, only Denon receivers offer stop.
        Action::Stop if !cfg.is_denon => {
            return Err(ServiceError::Blocked(
                "stop works on Marantz receivers only while HEOS plays".into(),
            ));
        }
        Action::Stop => send(avr, Command::Stop).await?,
        Action::Next => zone.press(RemoteKey::NextTrack).await?,
        Action::Previous => zone.press(RemoteKey::PreviousTrack).await?,
        Action::Volume(volume) => {
            let db = media_player::volume_db(volume);
            store(&expected.volume, db);
            zone.set_volume_db(db).await?;
        }
        Action::VolumeUp | Action::VolumeDown => {
            volume_step(avr, cfg, expected, matches!(action, Action::VolumeUp)).await?;
        }
        Action::MuteToggle => {
            let muted = fresh(&expected.mute).or(main.muted).unwrap_or(false);
            store(&expected.mute, !muted);
            zone.set_mute(!muted).await?;
        }
        Action::Mute(mute) => {
            store(&expected.mute, mute);
            zone.set_mute(mute).await?;
        }
        Action::CursorUp => send(avr, Command::CursorUp).await?,
        Action::CursorDown => send(avr, Command::CursorDown).await?,
        Action::CursorLeft => send(avr, Command::CursorLeft).await?,
        Action::CursorRight => send(avr, Command::CursorRight).await?,
        Action::CursorEnter => send(avr, Command::CursorEnter).await?,
        Action::Back => send(avr, Command::Back).await?,
        Action::Menu => {
            let open = state.device().settings_menu.unwrap_or(false);
            send(avr, Command::SetupMenu(!open)).await?;
        }
        Action::ContextMenu => send(avr, Command::Options).await?,
        Action::Info => send(avr, Command::Info).await?,
        Action::ChannelUp => avr.tuner().preset_up().await?,
        Action::ChannelDown => avr.tuner().preset_down().await?,
        Action::SelectSource(name) => {
            // The input switch works only if the receiver is on.
            zone.power_on().await?;
            let view = command_view(&state, cfg);
            select::select_option(avr, &view, SelectKind::InputSource, &name).await?;
        }
        Action::Simple(entry) => {
            // The typed call of the library checks the state and the range. A command without one
            // is sent as the text of the table.
            if let Some(run) = entry.typed() {
                let cx = simple_commands::Cx {
                    is_denon: cfg.is_denon,
                    state: &state,
                };
                run(avr, &cx).await?;
            } else {
                let text = entry.text(cfg.is_denon, &state);
                send(avr, Command::Raw(&text)).await?;
            }
        }
        Action::Raw(text) => send(avr, Command::Raw(&text)).await?,
        Action::SelectSoundMode(name) => {
            let view = command_view(&state, cfg);
            select::select_option(avr, &view, SelectKind::SoundMode, &name).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use select::Choices;

    fn choices(current: Option<&str>) -> Choices {
        Choices {
            options: vec!["a".into(), "b".into(), "c".into()],
            current: current.map(Into::into),
            numeric: false,
        }
    }

    #[test]
    fn step_moves_in_the_list() {
        assert_eq!(Some("b".into()), step(&choices(Some("a")), true, false));
        assert_eq!(Some("a".into()), step(&choices(Some("b")), false, false));
        assert_eq!(Some("a".into()), step(&choices(None), true, false));
        assert_eq!(Some("c".into()), step(&choices(None), false, false));
    }

    #[test]
    fn step_stops_or_cycles_at_the_ends() {
        assert_eq!(None, step(&choices(Some("c")), true, false));
        assert_eq!(Some("a".into()), step(&choices(Some("c")), true, true));
        assert_eq!(None, step(&choices(Some("a")), false, false));
        assert_eq!(Some("c".into()), step(&choices(Some("a")), false, true));
    }

    #[test]
    fn expected_values_expire() {
        let slot = Mutex::new(None);
        assert_eq!(None, fresh::<f32>(&slot));
        store(&slot, 1.5_f32);
        assert_eq!(Some(1.5), fresh(&slot));
        *slot.lock().expect("lock") = Instant::now()
            .checked_sub(EXPECTED_MAX_AGE * 2)
            .map(|at| (at, 2.0_f32));
        assert_eq!(None, fresh(&slot));
    }
}
