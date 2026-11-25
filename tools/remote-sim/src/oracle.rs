// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! A second connection to the receiver. It gives the values that the receiver has, to compare
//! them with the values that the driver shows.

use crate::Fail;
use denon_marantz::proto::source::telnet_to_display_source;
use denon_marantz::proto::telnet::EventZone;
use denon_marantz::proto::{DiracFilter, DiracSlot, Sleep};
use denon_marantz::{
    Ask, Avr, Feature, Query, QueryGroup, SettingStatus, StartOptions, State, TelnetEvent, Zone,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

/// The longest wait for the answer to the volume query. The receiver answers late while other
/// queries of the connection wait before them.
const VOLUME_ANSWER: Duration = Duration::from_secs(10);

/// How long more volume lines are collected after the first one.
const VOLUME_LAST_LINES: Duration = Duration::from_millis(300);

pub struct Oracle {
    avr: Arc<Avr>,
}

/// Expected value of an entity. `None` is shown as `--`.
pub type Expected = (&'static str, Option<String>);

impl Oracle {
    pub async fn connect(host: &str) -> Result<Self, Fail> {
        let avr = Arc::new(Avr::builder(host).connect().await?);
        avr.start_with(StartOptions::new()).await?;
        // The sound group and the Bluetooth level are not in the default groups.
        avr.telnet_refresh_group(QueryGroup::Sound).await?;
        avr.telnet_ask(&[Ask::Query(Query::BluetoothLevel)]).await?;
        Ok(Self { avr })
    }

    /// Read all values from the receiver again.
    pub async fn refresh(&self) -> Result<(), Fail> {
        self.avr.telnet_refresh().await?;
        self.avr.telnet_refresh_group(QueryGroup::Sound).await?;
        self.avr
            .telnet_ask(&[Ask::Query(Query::BluetoothLevel)])
            .await?;
        Ok(())
    }

    /// Ask again for the values that a speaker preset change changes: the receiver does not send
    /// them. The same values and wait as the driver (the `RELOAD_PRESET` entry of
    /// `RELOAD_QUERIES`, and `RELOAD_DELAY`, in `src/device/actor.rs`): the receiver needs time to
    /// take the preset.
    pub async fn ask_after_preset(&self) -> Result<(), Fail> {
        const QUERIES: [Query; 9] = [
            Query::DiracSlots,
            Query::DiracFilter,
            Query::ToneControl,
            Query::Bass,
            Query::Treble,
            Query::MultEq,
            Query::DynamicEq,
            Query::ReferenceLevel,
            Query::DynamicVolume,
        ];
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let info = self.avr.info();
        let asks: Vec<Ask<'_>> = QUERIES
            .into_iter()
            .filter(|&q| info.offers(q))
            .map(Ask::from)
            .collect();
        // An empty ask clears the filter of repeated events in the library.
        if !asks.is_empty() {
            self.avr.telnet_ask(&asks).await?;
        }
        Ok(())
    }

    /// What the library says about a setting, from the values of the second connection.
    pub fn status(&self, feature: Feature) -> SettingStatus {
        self.avr.setting_status(feature)
    }

    /// Ask the receiver for the volume of the main zone and wait for the answer. The volume is on
    /// the scale of the Remote (dB plus 80). A query clears the filter of repeated events in the
    /// library, so the answer always comes as an event.
    pub async fn fresh_volume(&self) -> Result<f64, Fail> {
        let mut events = self.avr.telnet_events();
        self.avr.telnet_ask(&[Ask::Query(Query::Volume)]).await?;
        let mut end = tokio::time::Instant::now() + VOLUME_ANSWER;
        let mut answered = false;
        loop {
            match tokio::time::timeout_at(end, events.recv()).await {
                Ok(Ok(event)) if answers_volume(&event) && !answered => {
                    answered = true;
                    end = tokio::time::Instant::now() + VOLUME_LAST_LINES;
                }
                Ok(Ok(_) | Err(RecvError::Lagged(_))) => {}
                Ok(Err(RecvError::Closed)) | Err(_) => break,
            }
        }
        if !answered {
            return Err(Fail::new("the receiver did not answer the volume query"));
        }
        remote_volume(&self.state())
            .ok_or_else(|| Fail::new("the receiver has no volume of the main zone"))
    }

    /// The volume of the main zone that the connection has, on the scale of the Remote. It is not
    /// asked again: see [`Oracle::fresh_volume`].
    pub fn volume(&self) -> Option<f64> {
        remote_volume(&self.state())
    }

    pub fn avr(&self) -> &Avr {
        &self.avr
    }

    pub fn state(&self) -> State {
        self.avr.snapshot()
    }

    pub async fn close(&self) {
        self.avr.telnet_disconnect().await;
    }
}

/// A volume line of the main zone. `MVMAX` is not an event.
fn answers_volume(event: &TelnetEvent) -> bool {
    event.zone == EventZone::Main && event.event.code() == "MV"
}

/// The volume of the main zone on the scale of the Remote: dB plus 80.
fn remote_volume(state: &State) -> Option<f64> {
    state
        .zone(Zone::Main)
        .volume
        .map(|v| f64::from(v.db()) + 80.0)
}

fn on_off(value: Option<bool>) -> Option<String> {
    value.map(|on| if on { "ON" } else { "OFF" }.to_owned())
}

/// Dirac filters in the option order of the driver.
const DIRAC_FILTERS: [DiracFilter; 4] = [
    DiracFilter::Slot1,
    DiracFilter::Slot2,
    DiracFilter::Slot3,
    DiracFilter::Off,
];

/// The Dirac filter as the driver shows it: the name that the filter was saved with, else
/// `Slot 1` to `Slot 3` or `Off`. A label that two options share gets the slot label in
/// parentheses, for example `Cinema (Slot 2)`.
fn dirac_label(slots: &[DiracSlot; 3], filter: Option<DiracFilter>) -> Option<String> {
    let name = |i: usize| {
        slots
            .get(i)
            .and_then(|slot| slot.name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(DIRAC_FILTERS[i].label())
    };
    let i = DIRAC_FILTERS.iter().position(|f| Some(*f) == filter)?;
    let own = name(i);
    let shared = (0..DIRAC_FILTERS.len()).any(|j| j != i && name(j) == own);
    Some(if shared {
        format!("{own} ({})", DIRAC_FILTERS[i].label())
    } else {
        own.to_owned()
    })
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
    let dirac = dirac_label(&device.dirac_slots, device.dirac_filter);
    let db = zone.volume.map(|v| f64::from(v.db()));
    let signal = &device.signal;
    let color = signal.video_color();
    let depth = signal.effective_pixel_depth();
    vec![
        ("select.input_source", source.clone()),
        ("select.dimmer", dimmer.clone()),
        ("select.eco_mode", eco.clone()),
        ("select.monitor_output", monitor.clone()),
        ("select.picture_mode", picture.clone()),
        ("select.digital_codec", codec.clone()),
        ("select.dirac_filter", dirac.clone()),
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
        ("sensor.dirac_filter", dirac),
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
        // The color space with BT.2020, as the driver shows it.
        (
            "sensor.colorspace_input",
            color.input.map(|c| c.to_string()),
        ),
        (
            "sensor.colorspace_output",
            color.output.map(|c| c.to_string()),
        ),
        (
            "sensor.pixel_depth_input",
            depth.input.map(|d| d.label().to_owned()),
        ),
        (
            "sensor.pixel_depth_output",
            depth.output.map(|d| d.label().to_owned()),
        ),
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
        // The level is 80 at 0 dB.
        (
            "sensor.bluetooth_level",
            device
                .bluetooth_level
                .map(|level| (i16::from(level) - 80).to_string()),
        ),
        (
            "sensor.sound_mode_category",
            zone.sound_mode_category.map(|c| c.label().to_owned()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use denon_marantz::proto::telnet::Message;

    fn state(lines: &[&str]) -> State {
        let mut state = State::new(true);
        for line in lines {
            let message = Message::parse(line).unwrap_or_else(|| panic!("not a message: {line}"));
            state.apply_message(&message);
        }
        state
    }

    fn dirac(lines: &[&str]) -> Option<String> {
        let state = state(lines);
        expected(&state)
            .into_iter()
            .find(|(name, _)| *name == "select.dirac_filter")
            .and_then(|(_, value)| value)
    }

    fn event(line: &str) -> TelnetEvent {
        let message = Message::parse(line).unwrap_or_else(|| panic!("not a message: {line}"));
        TelnetEvent {
            zone: message.zone,
            event: message.event,
            parameter: message.parameter.into(),
        }
    }

    #[test]
    fn only_a_main_zone_volume_line_answers_the_volume_query() {
        assert!(answers_volume(&event("MV45")));
        assert!(answers_volume(&event("MV455")));
        assert!(!answers_volume(&event("Z245")), "zone 2");
        assert!(!answers_volume(&event("MUON")));
        assert!(!answers_volume(&event("PWON")));
        // The maximum volume line is no event, or not a volume answer.
        assert!(Message::parse("MVMAX 98").is_none_or(|m| {
            !answers_volume(&TelnetEvent {
                zone: m.zone,
                event: m.event,
                parameter: m.parameter.into(),
            })
        }));
    }

    #[test]
    fn the_volume_is_on_the_scale_of_the_remote() {
        assert_eq!(None, remote_volume(&state(&["PWON"])));
        assert_eq!(Some(45.0), remote_volume(&state(&["PWON", "MV45"])));
        assert_eq!(Some(35.5), remote_volume(&state(&["PWON", "MV355"])));
    }

    #[test]
    fn the_dirac_filter_is_shown_as_the_driver_shows_it() {
        assert_eq!(None, dirac(&["PWON"]));
        assert_eq!(Some("Off".to_owned()), dirac(&["PWON", "PSDIRAC OFF"]));
        assert_eq!(Some("Slot 2".to_owned()), dirac(&["PWON", "PSDIRAC 2"]));
        let named = ["PWON", "OPDLSSL3NAME Music", "PSDIRAC 3"];
        assert_eq!(Some("Music".to_owned()), dirac(&named));
        let same = [
            "PWON",
            "OPDLSSL1NAME Cinema",
            "OPDLSSL2NAME Cinema",
            "PSDIRAC 2",
        ];
        assert_eq!(Some("Cinema (Slot 2)".to_owned()), dirac(&same));
        let spaces = ["PWON", "OPDLSSL1NAME    ", "PSDIRAC 1"];
        assert_eq!(Some("Slot 1".to_owned()), dirac(&spaces));
        let padded = ["PWON", "OPDLSSL2NAME Dirac ART      ", "PSDIRAC 2"];
        assert_eq!(Some("Dirac ART".to_owned()), dirac(&padded));
        let off = ["PWON", "OPDLSSL1NAME Off"];
        assert_eq!(
            Some("Off (Slot 1)".to_owned()),
            dirac(&[&off[..], &["PSDIRAC 1"]].concat())
        );
        assert_eq!(
            Some("Off (Off)".to_owned()),
            dirac(&[&off[..], &["PSDIRAC OFF"]].concat())
        );
    }
}
