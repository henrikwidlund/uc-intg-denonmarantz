// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Entities of one receiver: ids, definitions and attributes.
//!
//! Entity ids are the ones of the Python integration: `media_player.<key>`, `remote.<key>`,
//! `sensor.<kind>.<key>` and `select.<kind>.<key>`. The key is the serial number of the receiver.
//! The driver has more entities than the Python integration: the other selects and sensors, the
//! switches (`switch.<kind>.<key>`) and the numbers (`number.<kind>.<key>`).

pub mod media_player;
pub mod number;
pub mod remote;
pub mod select;
pub mod sensor;
pub mod simple_commands;
pub mod switch;

#[cfg(test)]
mod alloc_tests;
#[cfg(test)]
mod output_tests;

use crate::config_store::DeviceConfig;
use denon_marantz::State;
use denon_marantz::heos::PlayerState;
use serde_json::{Map, Value};
use std::collections::HashMap;
use uc_api::EntityType;
use uc_api::intg::AvailableIntgEntity;

/// What the entities show: the receiver state and the driver configuration.
pub struct View<'a> {
    pub state: &'a State,
    pub cfg: &'a DeviceConfig,
    /// The receiver is connected.
    pub connected: bool,
    /// Address of the picture of what plays.
    pub artwork_url: Option<&'a str>,
    /// Network audio was paused with the pause command. The receiver does not tell a pause from a
    /// stop: the Python integration shows `PAUSED` in the same way.
    pub paused: bool,
    /// The option of the active sound mode. See `select::active_sound_mode`.
    pub sound_mode: Option<&'a str>,
    /// The sound mode groups are the choices while the receiver gave no list. With telnet, only
    /// after the answers of the start are in: until then, the list can still come.
    pub sound_groups: bool,
    /// The HEOS player, while HEOS is connected.
    pub heos: Option<&'a PlayerState>,
    /// Position in what plays on HEOS. It changes only when it jumps or the play state changes:
    /// the Remote counts on from `media_position_updated_at`.
    pub position: Option<media_player::Position>,
}

/// Select entities of a receiver. The names are the ones of the Python integration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::AsRefStr, strum::EnumString, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum SelectKind {
    SoundMode,
    InputSource,
    ReferenceLevelOffset,
    DynamicVolume,
    Dimmer,
    EcoMode,
    MonitorOutput,
    SpeakerPreset,
    PictureMode,
    DigitalCodec,
    DiracFilter,
    // Not in the Python integration.
    MultiEq,
    QuickSelect,
    TunerBand,
    BluetoothOutput,
    NetworkFavorite,
    TunerPreset,
}

#[cfg(test)]
impl SelectKind {
    /// The Python integration has this select.
    pub const fn python(self) -> bool {
        (self as u8) < (Self::MultiEq as u8)
    }
}

/// Switch entities of a receiver. They are not in the Python integration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::AsRefStr, strum::EnumString, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum SwitchKind {
    ToneControl,
    DynamicEq,
    #[strum(serialize = "trigger_1")]
    Trigger1,
    #[strum(serialize = "trigger_2")]
    Trigger2,
    #[strum(serialize = "trigger_3")]
    Trigger3,
    BluetoothTransmitter,
}

/// Number entities of a receiver. They are not in the Python integration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::AsRefStr, strum::EnumString, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum NumberKind {
    Bass,
    Treble,
}

/// Sensor entities of a receiver. The names are the ones of the Python integration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::AsRefStr, strum::EnumString, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum SensorKind {
    VolumeDb,
    VolumeAbsolute,
    SoundMode,
    InputSource,
    Mute,
    Dimmer,
    EcoMode,
    SleepTimer,
    AudioDelay,
    AudioSignal,
    AudioSamplingRate,
    MonitorOutput,
    VideoHdmiSignalIn,
    VideoHdmiSignalOut,
    InputChannels,
    OutputChannels,
    MaxResolution,
    HdrInput,
    HdrOutput,
    PixelDepthInput,
    PixelDepthOutput,
    MaxFrlInput,
    MaxFrlOutput,
    ColorspaceInput,
    ColorspaceOutput,
    PictureMode,
    TunerFrequency,
    AllZoneStereo,
    DigitalCodec,
    // Not in the Python integration.
    DiracFilter,
    ProLogicMode,
    SurroundBackMode,
    FrontHeight,
    DynamicCompression,
    AutoFlagDetect,
    DialogLevelAdjust,
    DialogLevel,
    AudysseyDsx,
    StageWidth,
    StageHeight,
    HeightGain,
    InstaPrevue,
    BluetoothLevel,
    SoundModeCategory,
}

#[cfg(test)]
impl SensorKind {
    /// The Python integration has this sensor.
    pub const fn python(self) -> bool {
        (self as u8) < (Self::DiracFilter as u8)
    }
}

/// Entity of a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    MediaPlayer,
    Remote,
    Select(SelectKind),
    Sensor(SensorKind),
    Switch(SwitchKind),
    Number(NumberKind),
}

impl EntityKind {
    /// Every entity that a receiver with this configuration has.
    pub fn all(cfg: &DeviceConfig) -> impl Iterator<Item = Self> + '_ {
        use strum::VariantArray;
        [Self::MediaPlayer, Self::Remote]
            .into_iter()
            .chain(
                SelectKind::VARIANTS
                    .iter()
                    .copied()
                    .filter(|k| select::available(cfg, *k))
                    .map(Self::Select),
            )
            .chain(
                SensorKind::VARIANTS
                    .iter()
                    .copied()
                    .filter(|k| sensor::available(cfg, *k))
                    .map(Self::Sensor),
            )
            .chain(
                SwitchKind::VARIANTS
                    .iter()
                    .copied()
                    .filter(|k| switch::available(cfg, *k))
                    .map(Self::Switch),
            )
            .chain(
                NumberKind::VARIANTS
                    .iter()
                    .copied()
                    .filter(|k| number::available(cfg, *k))
                    .map(Self::Number),
            )
    }

    pub const fn entity_type(self) -> EntityType {
        match self {
            Self::MediaPlayer => EntityType::MediaPlayer,
            Self::Remote => EntityType::Remote,
            Self::Select(_) => EntityType::Select,
            Self::Sensor(_) => EntityType::Sensor,
            Self::Switch(_) => EntityType::Switch,
            Self::Number(_) => EntityType::Number,
        }
    }

    /// Build the entity id for a device key.
    pub fn entity_id(self, key: &str) -> String {
        let mut id = String::new();
        self.write_entity_id(key, &mut id);
        id
    }

    /// Append the entity id for a device key to `out`.
    pub fn write_entity_id(self, key: &str, out: &mut String) {
        use std::fmt::Write;
        let ty = self.entity_type();
        let _ = match self {
            Self::MediaPlayer | Self::Remote => write!(out, "{ty}.{key}"),
            Self::Select(s) => write!(out, "{ty}.{}.{key}", s.as_ref()),
            Self::Sensor(s) => write!(out, "{ty}.{}.{key}", s.as_ref()),
            Self::Switch(s) => write!(out, "{ty}.{}.{key}", s.as_ref()),
            Self::Number(n) => write!(out, "{ty}.{}.{key}", n.as_ref()),
        };
    }

    /// Parse an entity id into the device key and the entity kind.
    pub fn parse(entity_id: &str) -> Option<(&str, Self)> {
        let (ty, rest) = entity_id.split_once('.')?;
        match ty {
            "media_player" => Some((rest, Self::MediaPlayer)),
            "remote" => Some((rest, Self::Remote)),
            "select" => {
                let (kind, key) = rest.split_once('.')?;
                Some((key, Self::Select(kind.parse().ok()?)))
            }
            "sensor" => {
                let (kind, key) = rest.split_once('.')?;
                Some((key, Self::Sensor(kind.parse().ok()?)))
            }
            "switch" => {
                let (kind, key) = rest.split_once('.')?;
                Some((key, Self::Switch(kind.parse().ok()?)))
            }
            "number" => {
                let (kind, key) = rest.split_once('.')?;
                Some((key, Self::Number(kind.parse().ok()?)))
            }
            _ => None,
        }
    }
}

/// Build the definition of one entity.
pub fn definition(cfg: &DeviceConfig, kind: EntityKind) -> AvailableIntgEntity {
    let entity = AvailableIntgEntity {
        entity_id: kind.entity_id(cfg.key()),
        device_id: None,
        entity_type: kind.entity_type(),
        device_class: None,
        name: name(&cfg.name, entity_name_suffix(kind)),
        icon: None,
        description: None,
        features: None,
        area: None,
        options: None,
        attributes: None,
    };
    match kind {
        EntityKind::MediaPlayer => media_player::definition(entity, cfg),
        EntityKind::Remote => remote::definition(entity, cfg),
        EntityKind::Select(_) => entity,
        EntityKind::Sensor(k) => sensor::definition(entity, k),
        EntityKind::Switch(_) => switch::definition(entity),
        EntityKind::Number(k) => number::definition(entity, k),
    }
}

/// Insert all attributes of an entity.
pub fn attributes(v: &View<'_>, kind: EntityKind, out: &mut Map<String, Value>) {
    match kind {
        EntityKind::MediaPlayer => media_player::attributes(v, out),
        EntityKind::Remote => {
            out.insert("state".into(), remote::state(v).into());
        }
        EntityKind::Select(k) => select::attributes(v, k, out),
        EntityKind::Sensor(k) => sensor::attributes(v, k, out),
        EntityKind::Switch(k) => switch::attributes(v, k, out),
        EntityKind::Number(k) => number::attributes(v, k, out),
    }
}

/// Insert the attributes of an entity, but only the current option of a select with many
/// options. See [`select::current_attributes`].
pub fn current_attributes(v: &View<'_>, kind: EntityKind, out: &mut Map<String, Value>) {
    match kind {
        EntityKind::Select(k) => select::current_attributes(v, k, out),
        _ => attributes(v, kind, out),
    }
}

const fn entity_name_suffix(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::MediaPlayer => "",
        EntityKind::Remote => "Remote",
        EntityKind::Select(s) => select::name(s),
        EntityKind::Sensor(s) => sensor::name(s),
        EntityKind::Switch(s) => switch::name(s),
        EntityKind::Number(n) => number::name(n),
    }
}

fn name(device: &str, suffix: &str) -> HashMap<String, String> {
    let name = if suffix.is_empty() {
        device.to_string()
    } else {
        format!("{device} {suffix}")
    };
    HashMap::from([("en".to_string(), name)])
}

/// The receiver is switched off or in standby.
pub const fn is_off(v: &View<'_>) -> bool {
    matches!(
        v.state.zone(denon_marantz::Zone::Main).power,
        Some(denon_marantz::proto::Power::Standby | denon_marantz::proto::Power::Off)
    )
}

/// Entity state of an entity that shows values.
pub const fn availability(v: &View<'_>) -> &'static str {
    if v.connected { "ON" } else { "UNAVAILABLE" }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn cfg(use_telnet: bool, dirac: bool) -> DeviceConfig {
        DeviceConfig {
            id: "SERIAL0000001".into(),
            id_kind: crate::config_store::IdKind::Serial,
            serial: None,
            mac: None,
            name: "AVR".into(),
            address: "192.0.2.1".into(),
            support_sound_mode: true,
            show_all_inputs: false,
            use_telnet,
            update_audyssey: false,
            zone2: false,
            zone3: false,
            volume_step: 0.5,
            timeout: 2000,
            is_denon: true,
            is_dirac_supported: Some(dirac),
            signal_poll_secs: 0,
            receiver_type: None,
            update_method: None,
            hdmi_output_over_http: None,
            quick_select_count: None,
            listed_queries: None,
            comm_api_version: None,
            has_tuner: None,
            tuner_presets_lettered: None,
            tuner: None,
            tuner_preset_names: Vec::new(),
            network_favorite_names: Vec::new(),
            info: None,
        }
    }

    fn count(c: &DeviceConfig) -> (usize, usize) {
        let all: Vec<_> = EntityKind::all(c).collect();
        // Only the entities of the Python integration.
        let selects = all
            .iter()
            .filter(|k| matches!(k, EntityKind::Select(s) if s.python()))
            .count();
        let sensors = all
            .iter()
            .filter(|k| matches!(k, EntityKind::Sensor(s) if s.python()))
            .count();
        (selects, sensors)
    }

    #[test]
    fn entity_counts_are_the_ones_of_the_python_integration() {
        assert_eq!((11, 29), count(&cfg(true, true)));
        assert_eq!((10, 29), count(&cfg(true, false)));
        assert_eq!((6, 7), count(&cfg(false, true)));
    }

    #[test]
    fn sensors_of_older_receivers_follow_the_generation() {
        use crate::config_store::CommApiVersion;
        let sensors = |is_denon: bool, use_telnet: bool, version: &str| {
            let c = DeviceConfig {
                comm_api_version: CommApiVersion::parse(version),
                is_denon,
                ..cfg(use_telnet, true)
            };
            EntityKind::all(&c)
                .filter_map(|k| match k {
                    EntityKind::Sensor(s) if sensor::feature(s).is_some() => Some(s),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(Vec::<SensorKind>::new(), sensors(true, true, "0301"));
        assert_eq!(vec![SensorKind::FrontHeight], sensors(false, true, "0301"));
        assert_eq!(Vec::<SensorKind>::new(), sensors(true, false, "0300"));
        let denon_2015 = sensors(true, true, "0300");
        assert!(denon_2015.contains(&SensorKind::DialogLevel));
        assert!(!denon_2015.contains(&SensorKind::SurroundBackMode));
        assert_eq!(12, sensors(true, true, "none").len());
    }

    #[test]
    fn http_mode_offers_what_the_receiver_reads_over_http() {
        use crate::config_store::{HttpReadMethod, ReceiverGeneration};
        let http = |receiver_type, update_method| DeviceConfig {
            receiver_type,
            update_method,
            ..cfg(false, true)
        };
        let extra = |c: &DeviceConfig| {
            EntityKind::all(c)
                .filter(|k| !EntityKind::all(&cfg(false, true)).any(|base| base == *k))
                .collect::<Vec<_>>()
        };
        let app_command = [
            EntityKind::Select(SelectKind::MultiEq),
            EntityKind::Switch(SwitchKind::ToneControl),
            EntityKind::Switch(SwitchKind::DynamicEq),
            EntityKind::Number(NumberKind::Bass),
            EntityKind::Number(NumberKind::Treble),
        ];
        let signal = [
            SensorKind::AudioSignal,
            SensorKind::AudioSamplingRate,
            SensorKind::VideoHdmiSignalIn,
            SensorKind::VideoHdmiSignalOut,
            SensorKind::InputChannels,
            SensorKind::OutputChannels,
        ]
        .map(EntityKind::Sensor);

        let x2016 = extra(&http(
            Some(ReceiverGeneration::AvrX2016),
            Some(HttpReadMethod::AppCommand),
        ));
        assert_eq!(app_command.len() + signal.len(), x2016.len(), "{x2016:?}");
        assert!(app_command.iter().chain(&signal).all(|k| x2016.contains(k)));

        let avr_x = extra(&http(
            Some(ReceiverGeneration::AvrX),
            Some(HttpReadMethod::AppCommand),
        ));
        assert_eq!(app_command.as_slice(), avr_x.as_slice());

        for (receiver_type, update_method) in [
            (
                Some(ReceiverGeneration::AvrX),
                Some(HttpReadMethod::StatusPages),
            ),
            (
                Some(ReceiverGeneration::Avr),
                Some(HttpReadMethod::StatusPages),
            ),
            (None, None),
        ] {
            let c = http(receiver_type, update_method);
            assert!(extra(&c).is_empty(), "{receiver_type:?} {update_method:?}");
        }
    }

    #[test]
    fn http_mode_offers_the_monitor_output_if_the_receiver_reads_it() {
        let base = cfg(false, true);
        let c = DeviceConfig {
            hdmi_output_over_http: Some(true),
            ..cfg(false, true)
        };
        let extra: Vec<EntityKind> = EntityKind::all(&c)
            .filter(|k| !EntityKind::all(&base).any(|b| b == *k))
            .collect();
        assert_eq!(
            vec![
                EntityKind::Select(SelectKind::MonitorOutput),
                EntityKind::Sensor(SensorKind::MonitorOutput)
            ],
            extra
        );
    }

    #[test]
    fn http_mode_leaves_out_what_the_receiver_does_not_list() {
        use crate::config_store::{HttpQueries, HttpReadMethod, ReceiverGeneration};
        let http = |listed_queries| DeviceConfig {
            receiver_type: Some(ReceiverGeneration::AvrX2016),
            update_method: Some(HttpReadMethod::AppCommand),
            listed_queries,
            ..cfg(false, true)
        };
        let all: Vec<EntityKind> = EntityKind::all(&http(None)).collect();
        let listed: Vec<EntityKind> = EntityKind::all(&http(Some(HttpQueries {
            tone_control: true,
            audyssey: true,
            video_info: true,
            audio_info: true,
            input_signal: true,
            active_speaker: true,
            output_settings: false,
        })))
        .collect();
        assert_eq!(all, listed);
        let missing: Vec<EntityKind> = all
            .iter()
            .copied()
            .filter(|k| !EntityKind::all(&http(Some(HttpQueries::default()))).any(|o| o == *k))
            .collect();
        assert_eq!(
            vec![
                EntityKind::Select(SelectKind::MultiEq),
                EntityKind::Sensor(SensorKind::AudioSignal),
                EntityKind::Sensor(SensorKind::AudioSamplingRate),
                EntityKind::Switch(SwitchKind::ToneControl),
                EntityKind::Switch(SwitchKind::DynamicEq),
                EntityKind::Number(NumberKind::Bass),
                EntityKind::Number(NumberKind::Treble),
            ],
            missing
        );
        // Telnet mode does not use the list.
        let telnet = DeviceConfig {
            listed_queries: Some(HttpQueries::default()),
            ..cfg(true, true)
        };
        assert_eq!(
            EntityKind::all(&cfg(true, true)).count(),
            EntityKind::all(&telnet).count()
        );
    }

    #[test]
    fn entity_ids_are_the_ones_of_the_python_integration() {
        let c = cfg(true, true);
        let ids: Vec<String> = EntityKind::all(&c).map(|k| k.entity_id(c.key())).collect();
        for id in [
            "media_player.SERIAL0000001",
            "remote.SERIAL0000001",
            "sensor.video_hdmi_signal_out.SERIAL0000001",
            "select.reference_level_offset.SERIAL0000001",
            "select.dirac_filter.SERIAL0000001",
        ] {
            assert!(ids.iter().any(|i| i == id), "{id}");
        }
    }

    #[test]
    fn entity_id_round_trip() {
        let c = cfg(true, true);
        for kind in EntityKind::all(&c) {
            let id = kind.entity_id("SERIAL0000001");
            assert_eq!(
                Some(("SERIAL0000001", kind)),
                EntityKind::parse(&id),
                "{id}"
            );
        }
    }

    #[test]
    fn invalid_entity_ids() {
        assert_eq!(None, EntityKind::parse("media_player"));
        assert_eq!(None, EntityKind::parse("select.KEY"));
        assert_eq!(None, EntityKind::parse("select.unknown.KEY"));
        assert_eq!(None, EntityKind::parse("light.KEY"));
    }
}
