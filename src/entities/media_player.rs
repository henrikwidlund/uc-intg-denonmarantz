// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Media player entity.

use crate::config_store::DeviceConfig;
use crate::entities::sensor::{self, absolute_volume};
use crate::entities::{View, is_off, select, simple_commands};
use crate::errors::ServiceError;
use denon_marantz::Zone;
use denon_marantz::proto::Power;
use denon_marantz::proto::source::is_playing_source;
use serde_json::{Map, Value, json};
use std::str::FromStr;
use uc_api::intg::AvailableIntgEntity;
use uc_api::{
    MediaContentType, MediaPlayerAttribute as Attr, MediaPlayerCommand as Cmd,
    MediaPlayerDeviceClass, MediaPlayerFeature as F,
};

/// Highest volume that the Remote can set: the volume in dB plus 80.
const MAX_VOLUME: f32 = 98.0;
/// Lowest volume of the receiver in dB.
const MIN_DB: f32 = -80.0;
/// Longest text for the receiver.
const MAX_RAW_LEN: usize = 28;

const FEATURES: &[F] = &[
    F::OnOff,
    F::Toggle,
    F::Volume,
    F::VolumeUpDown,
    F::MuteToggle,
    F::Mute,
    F::Unmute,
    F::PlayPause,
    F::Next,
    F::Previous,
    F::MediaTitle,
    F::MediaArtist,
    F::MediaAlbum,
    F::MediaImageUrl,
    F::MediaType,
    F::SelectSource,
    F::DPad,
    F::Menu,
    F::ContextMenu,
    F::Info,
    F::ChannelSwitcher,
];

/// Commands that every receiver has, in the names of the Python integration.
pub const BASE_COMMANDS: &[&str] = &[
    "play_pause",
    "stop",
    "next",
    "previous",
    "volume",
    "volume_up",
    "volume_down",
    "mute_toggle",
    "mute",
    "unmute",
    "select_source",
    "select_sound_mode",
    "cursor_up",
    "cursor_down",
    "cursor_left",
    "cursor_right",
    "cursor_enter",
    "back",
    "menu",
    "context_menu",
    "info",
];

pub fn definition(mut entity: AvailableIntgEntity, cfg: &DeviceConfig) -> AvailableIntgEntity {
    entity.device_class = Some(MediaPlayerDeviceClass::Receiver.as_ref().into());
    let mut features: Vec<String> = FEATURES.iter().map(|f| f.as_ref().to_string()).collect();
    if cfg.support_sound_mode {
        features.push(F::SelectSoundMode.as_ref().to_string());
    }
    if cfg.is_denon {
        features.push(F::Stop.as_ref().to_string());
    }
    entity.features = Some(features);
    let mut options = Map::new();
    options.insert(
        "simple_commands".into(),
        json!(simple_commands::supported(cfg)),
    );
    entity.options = Some(options);
    entity
}

/// Entity state of the media player.
pub fn state(v: &View<'_>) -> &'static str {
    if !v.connected {
        return "UNAVAILABLE";
    }
    match v.state.zone(Zone::Main).power {
        Some(Power::On) => {
            if v.paused {
                "PAUSED"
            } else if v.state.device().now_playing.is_playing() == Some(true) {
                "PLAYING"
            } else {
                "ON"
            }
        }
        Some(_) => "OFF",
        None => "UNKNOWN",
    }
}

/// Insert all attributes.
pub fn attributes(v: &View<'_>, out: &mut Map<String, Value>) {
    let zone = v.state.zone(Zone::Main);
    let device = v.state.device();
    let off = is_off(v);
    set(out, Attr::State, state(v).into());
    if let Some(volume) = zone.volume {
        set(out, Attr::Volume, absolute_volume(volume).into());
    }
    if let Some(muted) = zone.muted {
        set(out, Attr::Muted, muted.into());
    }

    let choices = select::choices(v, crate::entities::SelectKind::InputSource);
    set(out, Attr::SourceList, Value::from(choices.options));
    // The source and the media are empty while the receiver is off.
    let source = if off {
        String::new()
    } else {
        choices.current.unwrap_or_default()
    };
    set(out, Attr::Source, source.clone().into());
    if v.cfg.support_sound_mode {
        let choices = select::choices(v, crate::entities::SelectKind::SoundMode);
        set(out, Attr::SoundModeList, Value::from(choices.options));
        set(
            out,
            Attr::SoundMode,
            choices.current.unwrap_or_default().into(),
        );
    }

    // As in the Python integration: a source that does not play media shows its name as the title.
    // The tuner shows the frequency, the band and the station when there is no other text.
    let now = &device.now_playing;
    let playing_source = zone.source.as_deref().is_some_and(is_playing_source);
    let title = if playing_source {
        now.title.clone().or_else(|| sensor::tuner_text(device))
    } else {
        Some(source)
    };
    let artist = now
        .artist
        .clone()
        .or_else(|| device.tuner.band.map(|b| b.label().to_owned()));
    let album = now.album.clone().or_else(|| now.station.clone());
    let text = |value: Option<String>| -> Value {
        if off {
            String::new()
        } else {
            value.unwrap_or_default()
        }
        .into()
    };
    set(out, Attr::MediaTitle, text(title));
    set(out, Attr::MediaArtist, text(artist));
    set(out, Attr::MediaAlbum, text(album));
    let artwork = if off { None } else { v.artwork_url };
    set(out, Attr::MediaImageUrl, artwork.unwrap_or("").into());
    set(
        out,
        Attr::MediaType,
        if off { "" } else { media_type(v) }.into(),
    );
}

fn media_type(v: &View<'_>) -> &'static str {
    if v.state.device().now_playing.title.is_some() {
        MediaContentType::Music.as_str()
    } else {
        ""
    }
}

fn set(out: &mut Map<String, Value>, a: Attr, v: Value) {
    out.insert(a.as_ref().into(), v);
}

/// Action to run on the receiver.
#[derive(Debug, Clone)]
pub enum Action {
    On,
    Off,
    Toggle,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Volume as the Remote shows it: 0 to 98.
    Volume(u8),
    VolumeUp,
    VolumeDown,
    MuteToggle,
    Mute(bool),
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    CursorEnter,
    Back,
    Menu,
    ContextMenu,
    Info,
    ChannelUp,
    ChannelDown,
    SelectSource(String),
    SelectSoundMode(String),
    /// A command of the table.
    Simple(&'static simple_commands::Entry),
    /// Text for the receiver, as the Python integration sends unknown commands.
    Raw(String),
}

/// Volume in dB for a volume of the Remote.
pub fn volume_db(volume: u8) -> f32 {
    (f32::from(volume).min(MAX_VOLUME) + MIN_DB).max(MIN_DB)
}

/// Map a media player command to an action.
pub fn command(cmd_id: &str, params: Option<&Map<String, Value>>) -> Result<Action, ServiceError> {
    let Ok(cmd) = Cmd::from_str(cmd_id) else {
        return other_command(cmd_id);
    };
    let action = match cmd {
        Cmd::On => Action::On,
        Cmd::Off => Action::Off,
        Cmd::Toggle => Action::Toggle,
        Cmd::PlayPause => Action::PlayPause,
        Cmd::Stop => Action::Stop,
        Cmd::Previous => Action::Previous,
        Cmd::Next => Action::Next,
        Cmd::Volume => {
            let v = param_f64(params, "volume")?;
            if !(0.0..=100.0).contains(&v) {
                return Err(ServiceError::BadRequest("Invalid parameter: volume".into()));
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Action::Volume(v.round() as u8)
        }
        Cmd::VolumeUp => Action::VolumeUp,
        Cmd::VolumeDown => Action::VolumeDown,
        Cmd::MuteToggle => Action::MuteToggle,
        Cmd::Mute => Action::Mute(true),
        Cmd::Unmute => Action::Mute(false),
        Cmd::CursorUp => Action::CursorUp,
        Cmd::CursorDown => Action::CursorDown,
        Cmd::CursorLeft => Action::CursorLeft,
        Cmd::CursorRight => Action::CursorRight,
        Cmd::CursorEnter => Action::CursorEnter,
        Cmd::Back => Action::Back,
        Cmd::Menu => Action::Menu,
        Cmd::ContextMenu => Action::ContextMenu,
        Cmd::Info => Action::Info,
        Cmd::ChannelUp => Action::ChannelUp,
        Cmd::ChannelDown => Action::ChannelDown,
        Cmd::SelectSource => Action::SelectSource(param_str(params, "source")?.into()),
        Cmd::SelectSoundMode => Action::SelectSoundMode(param_str(params, "mode")?.into()),
        _ => return other_command(cmd_id),
    };
    Ok(action)
}

/// A command of the table, or text for the receiver. The receiver takes up to 28 characters of
/// ASCII text.
fn other_command(cmd_id: &str) -> Result<Action, ServiceError> {
    if let Some(entry) = simple_commands::get(cmd_id) {
        return Ok(Action::Simple(entry));
    }
    if cmd_id.len() > MAX_RAW_LEN || !cmd_id.bytes().all(|b| (0x20..=0x7f).contains(&b)) {
        return Err(ServiceError::BadRequest(format!(
            "Unknown command: {cmd_id}"
        )));
    }
    Ok(Action::Raw(cmd_id.to_owned()))
}

pub fn param_str<'a>(
    params: Option<&'a Map<String, Value>>,
    name: &str,
) -> Result<&'a str, ServiceError> {
    params
        .and_then(|p| p.get(name))
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::BadRequest(format!("Missing parameter: {name}")))
}

/// Number parameter. `NaN` and infinite values are rejected.
fn param_f64(params: Option<&Map<String, Value>>, name: &str) -> Result<f64, ServiceError> {
    let value = params
        .and_then(|p| p.get(name))
        .ok_or_else(|| ServiceError::BadRequest(format!("Missing parameter: {name}")))?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|v: &f64| v.is_finite())
        .ok_or_else(|| ServiceError::BadRequest(format!("Invalid parameter: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_maps_to_db() {
        assert!((volume_db(0) - -80.0).abs() < f32::EPSILON);
        assert!((volume_db(40) - -40.0).abs() < f32::EPSILON);
        assert!((volume_db(98) - 18.0).abs() < f32::EPSILON);
        assert!((volume_db(100) - 18.0).abs() < f32::EPSILON);
    }

    #[test]
    fn commands_map_to_actions() {
        assert!(matches!(command("on", None), Ok(Action::On)));
        let params = json!({ "volume": 40 });
        assert!(matches!(
            command("volume", params.as_object()),
            Ok(Action::Volume(40))
        ));
        assert!(command("volume", None).is_err());
        assert!(matches!(command("OUTPUT_1", None), Ok(Action::Simple(_))));
        // Unknown text goes to the receiver, if it can take it.
        assert!(matches!(command("PWON", None), Ok(Action::Raw(_))));
        assert!(command(&"X".repeat(29), None).is_err());
        assert!(command("caf\u{e9}", None).is_err());
    }
}
