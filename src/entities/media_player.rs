// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Media player entity.

use crate::config_store::{DeviceConfig, Feature};
use crate::entities::sensor::{self, absolute_volume};
use crate::entities::{View, is_off, select, simple_commands};
use crate::errors::ServiceError;
use denon_marantz::heos::{MediaType, NowPlaying, PlayState, PlayerState, RepeatMode};
use denon_marantz::proto::PlayingKind;
use denon_marantz::proto::Power;
use denon_marantz::proto::source::{is_playing_source, playing_kind};
use denon_marantz::{State, ValueRange, Zone};
use serde_json::{Map, Value, json};
use std::str::FromStr;
use uc_api::intg::AvailableIntgEntity;
use uc_api::{
    MediaContentType, MediaPlayerAttribute as Attr, MediaPlayerCommand as Cmd,
    MediaPlayerDeviceClass, MediaPlayerFeature as F, MediaPlayerRepeatMode,
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
];

/// Commands that every receiver has, in the names of the Python integration, for the simple
/// commands of the remote entity. Commands that need a parameter are left out: a simple command has
/// none.
pub const BASE_COMMANDS: &[&str] = &[
    "play_pause",
    "stop",
    "next",
    "previous",
    "volume_up",
    "volume_down",
    "mute_toggle",
    "mute",
    "unmute",
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
    // Channel up and down select the tuner presets.
    if cfg.offers(Feature::Tuner) {
        features.push(F::ChannelSwitcher.as_ref().to_string());
    }
    // HEOS stops on both brands.
    if cfg.is_denon || cfg.may_have_heos() {
        features.push(F::Stop.as_ref().to_string());
    }
    // The Remote keeps the features of an entity: a receiver that may have HEOS gets them before
    // HEOS answers.
    if cfg.may_have_heos() {
        features.push(F::MediaPosition.as_ref().to_string());
        features.push(F::MediaDuration.as_ref().to_string());
        features.push(F::Repeat.as_ref().to_string());
        features.push(F::Shuffle.as_ref().to_string());
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
            if let Some(play_state) = heos_player(v).and_then(|p| p.play_state) {
                return match play_state {
                    PlayState::Play => "PLAYING",
                    PlayState::Pause => "PAUSED",
                    PlayState::Stop => "ON",
                };
            }
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
    // An unknown value is sent as null, so the Remote does not keep an old one.
    set(out, Attr::Volume, zone.volume.map(absolute_volume).into());
    set(out, Attr::Muted, zone.muted.into());

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

    let text = |value: Option<String>| -> Value {
        if off {
            String::new()
        } else {
            value.unwrap_or_default()
        }
        .into()
    };
    let heos = if off { None } else { heos_media(v) };
    if v.cfg.has_heos() {
        let position = heos.and(v.position).unwrap_or_default();
        set(out, Attr::MediaPosition, position.position.into());
        set(out, Attr::MediaDuration, position.duration.into());
        // The controller sets the time only if it sends a new position or state, see
        // [`position_timestamp`].
        set(out, Attr::MediaPositionUpdatedAt, Value::Null);
        let mode = if off {
            None
        } else {
            heos_player(v).and_then(PlayerState::play_mode)
        };
        let repeat = mode.map_or(MediaPlayerRepeatMode::Off, |m| remote_repeat(m.repeat));
        set(out, Attr::Repeat, repeat.as_ref().into());
        set(out, Attr::Shuffle, mode.is_some_and(|m| m.shuffle).into());
    }
    if let Some(media) = heos {
        let station = media.station.as_deref().filter(|t| !is_blank(t));
        let (title, album) = if is_blank(&media.song) {
            (station, non_blank(&media.album))
        } else {
            (Some(&*media.song), non_blank(&media.album).or(station))
        };
        set(out, Attr::MediaTitle, title.unwrap_or_default().into());
        set(out, Attr::MediaArtist, media.artist.as_ref().into());
        set(out, Attr::MediaAlbum, album.unwrap_or_default().into());
        set(out, Attr::MediaImageUrl, media.image_url.as_ref().into());
        let kind = match media.media_type {
            MediaType::Station => MediaContentType::Radio,
            _ => MediaContentType::Music,
        };
        set(out, Attr::MediaType, kind.as_str().into());
        return;
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

/// The repeat mode of HEOS as the Remote names it.
const fn remote_repeat(mode: RepeatMode) -> MediaPlayerRepeatMode {
    match mode {
        RepeatMode::Off => MediaPlayerRepeatMode::Off,
        RepeatMode::OnAll => MediaPlayerRepeatMode::All,
        RepeatMode::OnOne => MediaPlayerRepeatMode::One,
    }
}

/// Position in what plays on HEOS, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub position: u64,
    /// 0 for a live station.
    pub duration: u64,
}

/// Time for `media_position_updated_at`, like `2026-11-04T15:37:27.106292+00:00`.
pub fn position_timestamp() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%S%.6f+00:00")
        .to_string()
}

/// The HEOS player, while the main zone plays network audio and HEOS plays, is paused or has an
/// item. A stopped player without an item leaves the state to the receiver: for example Bluetooth
/// can play without HEOS.
fn heos_player<'a>(v: &View<'a>) -> Option<&'a PlayerState> {
    v.heos.filter(|player| uses_heos(v.state, player))
}

/// The main zone plays network audio and HEOS plays, is paused or has an item. Then the media
/// player shows HEOS, and play, pause, stop, next and previous go to HEOS.
pub fn uses_heos(state: &State, player: &PlayerState) -> bool {
    let net_audio = state
        .zone(Zone::Main)
        .source
        .as_deref()
        .is_some_and(|s| playing_kind(s) == Some(PlayingKind::NetAudio));
    net_audio
        && (matches!(player.play_state, Some(PlayState::Play | PlayState::Pause))
            || player.current_media().is_some_and(|m| !m.is_empty()))
}

/// What plays on HEOS. `None` while the item has no text: `AirPlay` sends such an item first.
fn heos_media<'a>(v: &View<'a>) -> Option<&'a NowPlaying<'static>> {
    heos_player(v)?.current_media().filter(|m| !m.is_empty())
}

fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn non_blank(text: &str) -> Option<&str> {
    (!is_blank(text)).then_some(text)
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
    /// Repeat mode of HEOS.
    Repeat(RepeatMode),
    /// Shuffle of HEOS on or off.
    Shuffle(bool),
    /// A command of the table.
    Simple(&'static simple_commands::Entry),
    /// Text for the receiver, as the Python integration sends unknown commands.
    Raw(String),
}

/// Volume in dB for a volume of the Remote. `scale` is the volume range that the receiver lists:
/// its highest value is the top of the scale. The library refuses a volume above it.
pub fn volume_db(volume: u8, scale: Option<ValueRange>) -> f32 {
    let max = scale.map_or(MAX_VOLUME, |range| {
        (f32::from(range.max_tenths) / 10.0).clamp(0.0, MAX_VOLUME)
    });
    (f32::from(volume).min(max) + MIN_DB).max(MIN_DB)
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
        Cmd::Repeat => {
            let mode = MediaPlayerRepeatMode::from_str(param_str(params, "repeat")?)
                .map_err(|_| ServiceError::BadRequest("Invalid parameter: repeat".into()))?;
            Action::Repeat(match mode {
                MediaPlayerRepeatMode::Off => RepeatMode::Off,
                MediaPlayerRepeatMode::All => RepeatMode::OnAll,
                MediaPlayerRepeatMode::One => RepeatMode::OnOne,
            })
        }
        Cmd::Shuffle => Action::Shuffle(
            params
                .and_then(|p| p.get("shuffle"))
                .and_then(Value::as_bool)
                .ok_or_else(|| ServiceError::BadRequest("Missing parameter: shuffle".into()))?,
        ),
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
        assert!((volume_db(0, None) - -80.0).abs() < f32::EPSILON);
        assert!((volume_db(40, None) - -40.0).abs() < f32::EPSILON);
        assert!((volume_db(98, None) - 18.0).abs() < f32::EPSILON);
        assert!((volume_db(100, None) - 18.0).abs() < f32::EPSILON);
    }

    #[test]
    fn volume_stays_at_or_below_the_listed_scale() {
        let scale = |max_tenths| {
            Some(ValueRange {
                min_tenths: 0,
                max_tenths,
                step_tenths: Some(5),
            })
        };
        assert!((volume_db(100, scale(980)) - 18.0).abs() < f32::EPSILON);
        assert!((volume_db(70, scale(600)) - -20.0).abs() < f32::EPSILON);
        assert!((volume_db(50, scale(600)) - -30.0).abs() < f32::EPSILON);
        assert!((volume_db(100, scale(-10)) - -80.0).abs() < f32::EPSILON);
    }

    #[test]
    fn position_timestamp_has_microseconds_and_offset() {
        let ts = position_timestamp();
        let ts = ts.as_str();
        // Same shape as `2026-11-04T15:37:27.106292+00:00`.
        assert_eq!(32, ts.len(), "{ts}");
        assert!(ts.ends_with("+00:00"), "{ts}");
        assert_eq!(Some('.'), ts.chars().nth(19), "{ts}");
        assert!(ts.parse::<jiff::Timestamp>().is_ok(), "{ts}");
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

    #[test]
    fn repeat_and_shuffle_take_their_parameter() {
        let repeat = |mode: &str| command("repeat", json!({ "repeat": mode }).as_object());
        assert!(matches!(
            repeat("ALL"),
            Ok(Action::Repeat(RepeatMode::OnAll))
        ));
        assert!(matches!(
            repeat("ONE"),
            Ok(Action::Repeat(RepeatMode::OnOne))
        ));
        assert!(matches!(repeat("OFF"), Ok(Action::Repeat(RepeatMode::Off))));
        assert!(repeat("all").is_err());
        assert!(command("repeat", None).is_err());
        let shuffle = json!({ "shuffle": true });
        assert!(matches!(
            command("shuffle", shuffle.as_object()),
            Ok(Action::Shuffle(true))
        ));
        let text = json!({ "shuffle": "true" });
        assert!(command("shuffle", text.as_object()).is_err());
    }
}
