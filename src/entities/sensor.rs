// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Sensor entities. Names, values and units are the ones of the Python integration.

use crate::config_store::DeviceConfig;
use crate::entities::{SelectKind, SensorKind, View, availability, is_off, select};
use denon_marantz::proto::{InOut, Sleep};
use denon_marantz::{Volume, Zone};
use serde_json::{Map, Value, json};
use uc_api::SensorDeviceClass;
use uc_api::intg::AvailableIntgEntity;

/// Shown for a value that is not known, and while the receiver is off.
pub const EMPTY: &str = "--";

pub fn definition(mut entity: AvailableIntgEntity, kind: SensorKind) -> AvailableIntgEntity {
    let class = if kind == SensorKind::Mute {
        SensorDeviceClass::Binary
    } else {
        SensorDeviceClass::Custom
    };
    entity.device_class = Some(class.as_ref().into());
    let mut options = Map::new();
    match kind {
        SensorKind::VolumeDb => {
            options.insert("custom_unit".into(), "dB".into());
            options.insert("decimals".into(), 1.into());
        }
        SensorKind::VolumeAbsolute => {
            options.insert("decimals".into(), 1.into());
        }
        SensorKind::AudioDelay => {
            options.insert("custom_unit".into(), "ms".into());
            options.insert("decimals".into(), 0.into());
        }
        SensorKind::AudioSamplingRate => {
            options.insert("custom_unit".into(), "kHz".into());
        }
        _ => {}
    }
    entity.options = Some(options);
    entity
}

pub const fn name(kind: SensorKind) -> &'static str {
    match kind {
        SensorKind::VolumeDb => "Volume dB",
        SensorKind::VolumeAbsolute => "Absolute Volume",
        SensorKind::SoundMode => "Audio Output",
        SensorKind::InputSource => "Input Source",
        SensorKind::Mute => "Mute Status",
        SensorKind::Dimmer => "Dimmer",
        SensorKind::EcoMode => "Eco Mode",
        SensorKind::SleepTimer => "Sleep Timer",
        SensorKind::AudioDelay => "Audio Delay",
        SensorKind::AudioSignal => "Audio Signal",
        SensorKind::AudioSamplingRate => "Audio Sampling Rate",
        SensorKind::MonitorOutput => "Video Out Port",
        SensorKind::VideoHdmiSignalIn => "Video In Format",
        SensorKind::VideoHdmiSignalOut => "Video Out Format",
        SensorKind::InputChannels => "Input Channels",
        SensorKind::OutputChannels => "Output Channels",
        SensorKind::MaxResolution => "Resolution/Bandwidth",
        SensorKind::HdrInput => "HDR Input",
        SensorKind::HdrOutput => "HDR Output",
        SensorKind::PixelDepthInput => "Pixel Depth Input",
        SensorKind::PixelDepthOutput => "Pixel Depth Output",
        SensorKind::MaxFrlInput => "Max FRL Input",
        SensorKind::MaxFrlOutput => "Max FRL Output",
        SensorKind::ColorspaceInput => "Colorspace Input",
        SensorKind::ColorspaceOutput => "Colorspace Output",
        SensorKind::PictureMode => "Picture Mode",
        SensorKind::TunerFrequency => "Tuner Frequency",
        SensorKind::AllZoneStereo => "All Zone Stereo",
        SensorKind::DigitalCodec => "Digital Codec",
        SensorKind::DiracFilter => "Active Dirac Filter",
    }
}

/// The first seven sensors always exist, the others only with a telnet connection. The signal
/// sensors that `Avr::update_signal_and_audyssey` reads also come over HTTP.
pub const fn available(cfg: &DeviceConfig, kind: SensorKind) -> bool {
    if matches!(kind, SensorKind::DiracFilter) {
        return cfg.use_telnet && cfg.has_dirac();
    }
    cfg.use_telnet
        || (cfg.reads_signal()
            && matches!(
                kind,
                SensorKind::AudioSignal
                    | SensorKind::AudioSamplingRate
                    | SensorKind::VideoHdmiSignalIn
                    | SensorKind::VideoHdmiSignalOut
                    | SensorKind::InputChannels
                    | SensorKind::OutputChannels
            ))
        || matches!(
            kind,
            SensorKind::VolumeDb
                | SensorKind::VolumeAbsolute
                | SensorKind::SoundMode
                | SensorKind::InputSource
                | SensorKind::Mute
                | SensorKind::Dimmer
                | SensorKind::EcoMode
        )
}

/// Insert the attributes of a sensor.
pub fn attributes(v: &View<'_>, kind: SensorKind, out: &mut Map<String, Value>) {
    out.insert("state".into(), availability(v).into());
    let (value, unit) = value(v, kind);
    out.insert("value".into(), value);
    if let Some(unit) = unit {
        out.insert("unit".into(), unit.into());
    }
}

/// Volume of the Remote: the dB value plus 80, from 0 to 98.
pub fn absolute_volume(volume: Volume) -> f64 {
    (f64::from(volume.db()) + 80.0).clamp(0.0, 98.0)
}

fn text<T: ToString>(v: Option<T>) -> Value {
    json!(v.map_or_else(|| EMPTY.to_owned(), |v| v.to_string()))
}

fn pick<T, R>(io: &InOut<T>, input: bool, f: impl FnOnce(&T) -> R) -> Option<R> {
    let value = if input { &io.input } else { &io.output };
    value.as_ref().map(f)
}

/// Value and unit of a sensor. A unit of `None` is not sent.
fn value(v: &View<'_>, kind: SensorKind) -> (Value, Option<&'static str>) {
    let zone = v.state.zone(Zone::Main);
    let device = v.state.device();
    let signal = &device.signal;
    if is_off(v) {
        // These keep their value while the receiver is off. All others are empty.
        match kind {
            SensorKind::Mute => return ("off".into(), None),
            SensorKind::MonitorOutput | SensorKind::InputSource => {}
            SensorKind::SleepTimer => return (EMPTY.into(), Some("")),
            _ => return (EMPTY.into(), None),
        }
    }
    let value = match kind {
        SensorKind::VolumeDb => zone
            .volume
            .map_or_else(|| EMPTY.into(), |x| json!(round1(f64::from(x.db())))),
        SensorKind::VolumeAbsolute => zone
            .volume
            .map_or_else(|| EMPTY.into(), |x| json!(absolute_volume(x))),
        SensorKind::SoundMode => text(signal.audio_sound.as_deref().or(zone.sound_mode.as_deref())),
        SensorKind::InputSource => text(
            zone.source
                .as_deref()
                .map(|source| select::source_label(v, source)),
        ),
        SensorKind::Mute => json!(if zone.muted == Some(true) {
            "on"
        } else {
            "off"
        }),
        SensorKind::Dimmer => text(device.dimmer.map(|x| x.label())),
        SensorKind::EcoMode => text(device.eco_mode.map(|x| x.label())),
        SensorKind::SleepTimer => {
            return match zone.sleep {
                Some(Sleep::Minutes(m)) => (json!(m), Some("min")),
                // The empty unit clears the minutes.
                _ => ("Off".into(), Some("")),
            };
        }
        SensorKind::AudioDelay => device
            .sound_settings
            .audio_delay
            .map_or_else(|| EMPTY.into(), |ms| json!(ms)),
        SensorKind::AudioSignal => text(signal.audio_signal.as_deref()),
        SensorKind::AudioSamplingRate => text(signal.sampling_rate.as_deref()),
        SensorKind::MonitorOutput => text(device.video_settings.hdmi_output.map(|x| x.label())),
        SensorKind::VideoHdmiSignalIn => text(pick(&signal.video, true, Clone::clone)),
        SensorKind::VideoHdmiSignalOut => text(pick(&signal.video, false, Clone::clone)),
        SensorKind::InputChannels => text(pick(&signal.channels, true, ToString::to_string)),
        SensorKind::OutputChannels => text(pick(&signal.channels, false, ToString::to_string)),
        SensorKind::MaxResolution => text(signal.max_hdmi_resolution.as_deref()),
        SensorKind::HdrInput => text(pick(&signal.hdr, true, Clone::clone)),
        SensorKind::HdrOutput => text(pick(&signal.hdr, false, Clone::clone)),
        // An input depth that the receiver reports as unknown while a signal comes in shows the
        // output depth.
        SensorKind::PixelDepthInput => {
            text(pick(&signal.effective_pixel_depth(), true, |x| x.label()))
        }
        SensorKind::PixelDepthOutput => text(pick(&signal.pixel_depth, false, |x| x.label())),
        SensorKind::MaxFrlInput => text(pick(&signal.max_frl, true, Clone::clone)),
        SensorKind::MaxFrlOutput => text(pick(&signal.max_frl, false, Clone::clone)),
        // The color space with BT.2020, for example `4:2:2/BT.2020`.
        SensorKind::ColorspaceInput => text(signal.video_color().input),
        SensorKind::ColorspaceOutput => text(signal.video_color().output),
        SensorKind::PictureMode => text(device.picture_settings.mode.map(|x| x.label())),
        SensorKind::TunerFrequency => text(tuner_text(device)),
        SensorKind::AllZoneStereo => text(
            device
                .all_zone_stereo
                .map(|on| if on { "ON" } else { "OFF" }),
        ),
        SensorKind::DigitalCodec => text(device.digital_codec.map(|x| x.label())),
        SensorKind::DiracFilter => text(select::choices(v, SelectKind::DiracFilter).current),
    };
    let unit = match kind {
        SensorKind::VolumeDb => Some("dB"),
        SensorKind::AudioDelay => Some("ms"),
        _ => None,
    };
    (value, unit)
}

/// The frequency of the tuner with its unit.
pub fn tuner_text(device: &denon_marantz::DeviceState) -> Option<String> {
    device.tuner.frequency.map(|f| {
        let h = f.hundredths();
        // Values above 050000 are AM in kHz, the others FM in MHz.
        if h > 50_000 {
            format!("{} kHz", h / 100)
        } else {
            format!("{}.{:02} MHz", h / 100, h % 100)
        }
    })
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_volume_is_db_plus_80() {
        let v = Volume::from_db_rounded(-40.0).expect("volume");
        assert!((absolute_volume(v) - 40.0).abs() < f64::EPSILON);
        let v = Volume::from_db_rounded(-80.0).expect("volume");
        assert!(absolute_volume(v).abs() < f64::EPSILON);
        let v = Volume::from_db_rounded(-40.5).expect("volume");
        assert!((absolute_volume(v) - 39.5).abs() < f64::EPSILON);
        let v = Volume::from_db_rounded(18.0).expect("volume");
        assert!((absolute_volume(v) - 98.0).abs() < f64::EPSILON);
    }
}
