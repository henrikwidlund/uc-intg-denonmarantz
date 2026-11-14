// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Simple commands of the media player and remote entities. The names are the ones of the Python
//! integration, so activities and macros keep working.
//!
//! The table is `resources/commands.tsv`: name, protocol (`all` or `telnet`), brand (`all`,
//! `denon` or `marantz`) and the text for telnet. A text with a `/` between two words toggles:
//! the first word is sent when the second one is the current value. A text of two key codes
//! (`Denon / Marantz`) has one code for each brand.

mod typed;

pub use typed::{CallFuture, Cx, Run};

use crate::config_store::DeviceConfig;
use denon_marantz::State;
use std::collections::HashMap;
use std::sync::LazyLock;

const TABLE: &str = include_str!("../../resources/commands.tsv");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Brand {
    All,
    Denon,
    Marantz,
}

#[derive(Debug)]
pub struct Entry {
    pub name: &'static str,
    telnet_only: bool,
    brand: Brand,
    text: &'static str,
}

static ENTRIES: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    TABLE
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?;
            let telnet_only = parts.next()? == "telnet";
            let brand = match parts.next()? {
                "denon" => Brand::Denon,
                "marantz" => Brand::Marantz,
                _ => Brand::All,
            };
            let text = parts.next()?;
            Some(Entry {
                name,
                telnet_only,
                brand,
                text,
            })
        })
        .collect()
});

static INDEX: LazyLock<HashMap<&str, &Entry>> =
    LazyLock::new(|| ENTRIES.iter().map(|e| (e.name, e)).collect());

impl Entry {
    /// The typed call of the library for this command. A command without one is sent as raw text.
    pub fn typed(&self) -> Option<Run> {
        typed::lookup(self.name)
    }

    /// The command is for Denon receivers, or for both brands.
    #[cfg(test)]
    pub(crate) fn for_denon(&self) -> bool {
        self.brand != Brand::Marantz
    }

    /// The text of the table, for the tests.
    #[cfg(test)]
    pub(crate) const fn text_for_test(&self) -> &'static str {
        self.text
    }

    /// The command is for Marantz receivers, or for both brands.
    #[cfg(test)]
    pub(crate) fn for_marantz(&self) -> bool {
        self.brand != Brand::Denon
    }

    /// The receiver has the command: it has the brand, and telnet is on if the command needs it.
    const fn is_supported(&self, cfg: &DeviceConfig) -> bool {
        (cfg.use_telnet || !self.telnet_only)
            && match self.brand {
                Brand::All => true,
                Brand::Denon => cfg.is_denon,
                Brand::Marantz => !cfg.is_denon,
            }
    }

    /// The text to send to the receiver now.
    pub fn text(&self, is_denon: bool, state: &State) -> String {
        if let Some((denon, marantz)) = self.text.split_once(" / ") {
            return (if is_denon { denon } else { marantz }).to_owned();
        }
        match toggle_current(self.name, state) {
            Some(current) => toggled(self.text, Some(current)),
            None if self.name.ends_with("_TOGGLE") => {
                // The state is not known: the first value.
                toggled(self.text, None)
            }
            None => self.text.to_owned(),
        }
    }
}

/// The names of the commands that a receiver has.
pub fn supported(cfg: &DeviceConfig) -> Vec<&'static str> {
    ENTRIES
        .iter()
        .filter(|e| e.is_supported(cfg))
        .map(|e| e.name)
        .collect()
}

pub fn get(name: &str) -> Option<&'static Entry> {
    INDEX.get(name).copied()
}

/// Text of a toggle: `PSGEQ ON/OFF` gives `PSGEQ OFF` if the current value is `ON`. Without a
/// known value it gives the first one.
fn toggled(text: &str, current: Option<&str>) -> String {
    let Some((head, values)) = text.rsplit_once(' ') else {
        return text.to_owned();
    };
    let Some((first, second)) = values.split_once('/') else {
        return text.to_owned();
    };
    let value = if current == Some(first) {
        second
    } else {
        first
    };
    format!("{head} {value}")
}

/// The current value of what a toggle command changes, in the words of its text.
fn toggle_current(name: &str, state: &State) -> Option<&'static str> {
    let device = state.device();
    let sound = &device.sound_settings;
    let on_off = |v: Option<bool>| v.map(|on| if on { "ON" } else { "OFF" });
    match name {
        "SPEAKER_PRESET_TOGGLE" => device.speaker_preset.map(|p| p.code()).map(|c| match c {
            "1" => "1",
            _ => "2",
        }),
        "BT_TRANSMITTER_TOGGLE" => on_off(device.bluetooth_transmitter),
        "BT_OUTPUT_MODE_TOGGLE" => device.bluetooth_output.map(|m| match m.code() {
            "SP" => "SP",
            _ => "BT",
        }),
        "TACTILE_TRANSDUCER_TOGGLE" => on_off(sound.tactile_transducer),
        "GRAPHIC_EQ_TOGGLE" => on_off(sound.graphic_eq),
        "HEADPHONE_EQ_TOGGLE" => on_off(sound.headphone_eq),
        "SOUND_MODE_NEURAL_X_TOGGLE" => on_off(sound.neural_x),
        "SOUND_MODE_IMAX_TOGGLE" => sound.imax.map(|m| match m.code() {
            "OFF" => "OFF",
            _ => "AUTO",
        }),
        "IMAX_AUDIO_SETTINGS_TOGGLE" => sound.imax_audio.map(|m| match m.code() {
            "MANUAL" => "MANUAL",
            _ => "AUTO",
        }),
        "CINEMA_EQ_TOGGLE" => sound
            .cinema_eq
            .map(|on| if on { "EQ.ON" } else { "EQ.OFF" }),
        "CENTER_SPREAD_TOGGLE" => on_off(sound.center_spread),
        "LOUDNESS_MANAGEMENT_TOGGLE" => on_off(sound.loudness_management),
        "SPEAKER_VIRTUALIZER_TOGGLE" => on_off(sound.speaker_virtualizer),
        "DYNAMIC_EQ_TOGGLE" => on_off(device.dynamic_eq),
        "AUDYSSEY_LFC_TOGGLE" => on_off(device.lfc),
        "SUBWOOFER_TOGGLE" => on_off(sound.subwoofer),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(use_telnet: bool, is_denon: bool) -> DeviceConfig {
        DeviceConfig {
            id: "S".into(),
            name: "N".into(),
            address: "h".into(),
            support_sound_mode: true,
            show_all_inputs: false,
            use_telnet,
            update_audyssey: false,
            zone2: false,
            zone3: false,
            volume_step: 0.5,
            timeout: 2000,
            is_denon,
            is_dirac_supported: None,
            signal_poll_secs: 0,
            receiver_type: None,
            update_method: None,
        }
    }

    #[test]
    fn the_counts_are_the_ones_of_the_python_integration() {
        assert_eq!(362, supported(&cfg(true, true)).len());
        assert_eq!(374, supported(&cfg(true, false)).len());
        assert_eq!(ENTRIES.len(), 387);
        assert!(supported(&cfg(false, true)).len() < 362);
    }

    #[test]
    fn every_text_fits_the_receiver() {
        for e in ENTRIES.iter() {
            assert!(!e.text.is_empty(), "{}", e.name);
            assert!(e.text.is_ascii(), "{}", e.name);
            for part in e.text.split(" / ") {
                assert!(part.len() <= 28, "{}", e.name);
            }
        }
    }

    #[test]
    fn plain_and_key_code_texts() {
        let state = State::new(true);
        assert_eq!("VSMONI1", get("OUTPUT_1").expect("cmd").text(true, &state));
        let cec = get("HDMI_CEC_ON").expect("cmd");
        assert_eq!("RCKSK0410826", cec.text(true, &state));
        assert_eq!("RCRC51608408", cec.text(false, &state));
        // Denon and Marantz have their own text for the custom picture mode.
        let custom = get("PICTURE_MODE_CUSTOM").expect("cmd");
        assert_eq!("PVCTM", custom.text(true, &state));
        assert_eq!("PVCSTM", custom.text(false, &state));
    }

    /// Texts that are checked against what the Python
    /// integration sends.
    #[test]
    fn corrected_texts() {
        let state = State::new(true);
        for (name, text) in [
            ("VIDEO_PROCESSING_MODE_MOVIE", "VSVPMMOVI"),
            ("MULTIEQ_BYPASS_LR", "PSMULTEQ:BYP.LR"),
            ("IMAX_HPF_40HZ", "PSIMAXHPF 040"),
            ("IMAX_LPF_90HZ", "PSIMAXLPF 090"),
            ("TRIGGER3_ON", "TR3 ON"),
            ("TRIGGER3_OFF", "TR3 OFF"),
            ("BASS_DOWN", "PSBAS DOWN"),
        ] {
            assert_eq!(text, get(name).expect("cmd").text(true, &state), "{name}");
        }
    }

    #[test]
    fn toggle_text() {
        assert_eq!("PSGEQ OFF", toggled("PSGEQ ON/OFF", Some("ON")));
        assert_eq!("PSGEQ ON", toggled("PSGEQ ON/OFF", Some("OFF")));
        assert_eq!("PSGEQ ON", toggled("PSGEQ ON/OFF", None));
        assert_eq!("SPPR 2", toggled("SPPR 1/2", Some("1")));
        assert_eq!("SSTTR ON", toggled("SSTTR ON/OFF", Some("OFF")));
        let state = State::new(true);
        assert_eq!(
            "BTTX ON",
            get("BT_TRANSMITTER_TOGGLE")
                .expect("cmd")
                .text(true, &state)
        );
    }
}
