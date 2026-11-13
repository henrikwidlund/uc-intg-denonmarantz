// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! A second connection to the receiver. It gives the values that the receiver has, to compare
//! them with the values that the driver shows.

use crate::Fail;
use denon_marantz::proto::Sleep;
use denon_marantz::proto::source::telnet_to_display_source;
use denon_marantz::{Avr, Feature, QueryGroup, SettingStatus, StartOptions, State, Zone};
use std::sync::Arc;

pub struct Oracle {
    avr: Arc<Avr>,
}

/// Expected value of an entity. `None` is shown as `--`.
pub type Expected = (&'static str, Option<String>);

impl Oracle {
    pub async fn connect(host: &str) -> Result<Self, Fail> {
        let avr = Arc::new(Avr::builder(host).connect().await?);
        avr.start_with(StartOptions::new()).await?;
        // The sound group is not in the default groups.
        avr.telnet_refresh_group(QueryGroup::Sound).await?;
        Ok(Self { avr })
    }

    /// Read all values from the receiver again.
    pub async fn refresh(&self) -> Result<(), Fail> {
        self.avr.telnet_refresh().await?;
        self.avr.telnet_refresh_group(QueryGroup::Sound).await?;
        Ok(())
    }

    /// What the library says about a setting, from the values of the second connection.
    pub fn status(&self, feature: Feature) -> SettingStatus {
        self.avr.setting_status(feature)
    }

    pub fn state(&self) -> State {
        self.avr.snapshot()
    }

    pub async fn close(&self) {
        self.avr.telnet_disconnect().await;
    }
}

fn on_off(value: Option<bool>) -> Option<String> {
    value.map(|on| if on { "ON" } else { "OFF" }.to_owned())
}

fn label<T: Copy>(value: Option<T>, f: fn(T) -> &'static str) -> Option<String> {
    value.map(|v| f(v).to_owned())
}

/// The values that the entities must show. Keys are `select.<kind>` and `sensor.<kind>`.
pub fn expected(state: &State) -> Vec<Expected> {
    use denon_marantz::proto::{
        BluetoothOutputMode, DigitalCodec, DimmerMode, DynamicVolume, EcoMode, HdmiOutput, MultiEq,
        PictureMode, ReferenceLevelOffset, SpeakerPreset, Trigger, TunerBand,
    };
    let zone = state.zone(Zone::Main);
    let device = state.device();
    let source = zone.source.as_deref().map(|code| {
        device
            .sources
            .get(code)
            .and_then(|s| s.name.as_deref().or(s.default_name.as_deref()))
            .unwrap_or_else(|| telnet_to_display_source(code))
            .to_owned()
    });
    let dimmer = label(device.dimmer, DimmerMode::label);
    let eco = label(device.eco_mode, EcoMode::label);
    let monitor = label(device.video_settings.hdmi_output, HdmiOutput::label);
    let picture = label(device.picture_settings.mode, PictureMode::label);
    let codec = label(device.digital_codec, DigitalCodec::label);
    let db = zone.volume.map(|v| f64::from(v.db()));
    vec![
        ("select.input_source", source.clone()),
        ("select.dimmer", dimmer.clone()),
        ("select.eco_mode", eco.clone()),
        ("select.monitor_output", monitor.clone()),
        ("select.picture_mode", picture.clone()),
        ("select.digital_codec", codec.clone()),
        (
            "select.reference_level_offset",
            label(device.reference_level_offset, ReferenceLevelOffset::label),
        ),
        (
            "select.dynamic_volume",
            label(device.dynamic_volume, DynamicVolume::label),
        ),
        (
            "select.speaker_preset",
            device.speaker_preset.map(|p| match p {
                SpeakerPreset::One => "1".to_owned(),
                _ => "2".to_owned(),
            }),
        ),
        ("sensor.input_source", source),
        ("sensor.dimmer", dimmer),
        ("sensor.eco_mode", eco),
        ("sensor.monitor_output", monitor),
        ("sensor.picture_mode", picture),
        ("sensor.digital_codec", codec),
        (
            "sensor.mute",
            Some(
                if zone.muted == Some(true) {
                    "on"
                } else {
                    "off"
                }
                .to_owned(),
            ),
        ),
        ("sensor.volume_db", db.map(|d| format!("{d:.1}"))),
        ("select.multi_eq", label(device.multi_eq, MultiEq::label)),
        (
            "select.tuner_band",
            label(device.tuner.band, TunerBand::label),
        ),
        (
            "select.bluetooth_output",
            label(device.bluetooth_output, BluetoothOutputMode::label),
        ),
        ("switch.tone_control", on_off(device.tone_control)),
        ("switch.dynamic_eq", on_off(device.dynamic_eq)),
        ("switch.trigger_1", on_off(device.trigger(Trigger::One))),
        ("switch.trigger_2", on_off(device.trigger(Trigger::Two))),
        ("switch.trigger_3", on_off(device.trigger(Trigger::Three))),
        (
            "switch.bluetooth_transmitter",
            on_off(device.bluetooth_transmitter),
        ),
        ("number.bass", zone.bass.map(|l| l.db().to_string())),
        ("number.treble", zone.treble.map(|l| l.db().to_string())),
        (
            "sensor.volume_absolute",
            db.map(|d| format!("{:.1}", (d + 80.0).clamp(0.0, 98.0))),
        ),
        (
            "sensor.sleep_timer",
            Some(match zone.sleep {
                Some(Sleep::Minutes(m)) => m.to_string(),
                _ => "Off".to_owned(),
            }),
        ),
        (
            "sensor.all_zone_stereo",
            device
                .all_zone_stereo
                .map(|on| if on { "ON" } else { "OFF" }.to_owned()),
        ),
    ]
}
