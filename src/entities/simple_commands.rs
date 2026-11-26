// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Simple commands of the media player and remote entities. The names are the ones of the Python
//! integration, so activities and macros keep working, plus commands of older receivers.
//!
//! The table is `resources/commands.tsv`: name, protocol (`all` or `telnet`), brand (`all`,
//! `denon` or `marantz`), the text for telnet and, if only some receivers have the command, the
//! feature (see [`Feature::parse`]). A text with a `/` between two words toggles:
//! the first word is sent when the second one is the current value. A text of two key codes
//! (`Denon / Marantz`) has one code for each brand.

mod typed;

pub use typed::{CallFuture, Cx, Run};

use crate::config_store::{DeviceConfig, Feature};
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
    /// The quick select (smart select on Marantz) slot that the command uses.
    quick_select: Option<u8>,
    /// The feature that only some receivers have.
    feature: Option<Feature>,
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
                quick_select: quick_select_slot(name),
                feature: parts.next().and_then(Feature::parse),
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

    /// The receiver has the command: it has the brand, the quick select slot and the feature, the
    /// library offers the text, and telnet is on if the command needs it.
    fn is_supported(&self, cfg: &DeviceConfig) -> bool {
        (cfg.use_telnet || !self.telnet_only)
            && match self.brand {
                Brand::All => true,
                Brand::Denon => cfg.is_denon,
                Brand::Marantz => !cfg.is_denon,
            }
            && self
                .quick_select
                .is_none_or(|slot| cfg.has_quick_select(slot))
            && self.feature.is_none_or(|feature| cfg.offers(feature))
            && cfg.offers_line(self.brand_text(cfg.is_denon))
    }

    /// The text for the brand. A toggle keeps both values.
    fn brand_text(&self, is_denon: bool) -> &'static str {
        match self.text.split_once(" / ") {
            Some((denon, _)) if is_denon => denon,
            Some((_, marantz)) => marantz,
            None => self.text,
        }
    }

    /// The command sends one of two values, for example `PSGEQ ON/OFF`.
    pub fn toggles(&self) -> bool {
        self.name.ends_with("_TOGGLE") && self.text.contains('/') && !self.text.contains(" / ")
    }

    /// The text to send to the receiver now.
    pub fn text(&self, is_denon: bool, state: &State) -> String {
        if self.text.contains(" / ") {
            return self.brand_text(is_denon).to_owned();
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

/// The slot of a quick select or smart select command, for example 2 for `QUICK_SELECT_MEMORY_2`.
pub fn quick_select_slot(name: &str) -> Option<u8> {
    let rest = name
        .strip_prefix("QUICK_SELECT_")
        .or_else(|| name.strip_prefix("SMART_SELECT_"))?;
    rest.strip_prefix("MEMORY_").unwrap_or(rest).parse().ok()
}

/// The names of the commands that a receiver has.
pub fn supported(cfg: &DeviceConfig) -> Vec<&'static str> {
    ENTRIES
        .iter()
        .filter(|e| e.is_supported(cfg))
        .map(|e| e.name)
        .collect()
}

/// The receiver has the command. True for a name that is not in the table.
pub fn is_supported(name: &str, cfg: &DeviceConfig) -> bool {
    get(name).is_none_or(|e| e.is_supported(cfg))
}

pub fn get(name: &str) -> Option<&'static Entry> {
    INDEX.get(name).copied()
}

/// Text of a toggle: `PSGEQ ON/OFF` gives `PSGEQ OFF` if the current value is `ON`. Without a
/// known value it gives the first one. The values come after the last space or `:`.
fn toggled(text: &str, current: Option<&str>) -> String {
    let Some(at) = text.rfind([' ', ':']) else {
        return text.to_owned();
    };
    let (head, values) = text.split_at(at + 1);
    let Some((first, second)) = values.split_once('/') else {
        return text.to_owned();
    };
    let value = if current == Some(first) {
        second
    } else {
        first
    };
    format!("{head}{value}")
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
        "TACTILE_CHANNEL_TOGGLE" => on_off(sound.tactile_channel),
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
        "FRONT_HEIGHT_TOGGLE" => on_off(sound.front_height),
        "AUTO_FLAG_DETECT_TOGGLE" => on_off(sound.auto_flag_detect),
        "DIALOG_LEVEL_ADJUST_TOGGLE" => on_off(sound.dialog_level_adjust),
        "INSTA_PREVUE_TOGGLE" => on_off(device.insta_prevue),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denon_marantz::proto::spec_feature_of_line;

    fn cfg(use_telnet: bool, is_denon: bool) -> DeviceConfig {
        DeviceConfig {
            id: "S".into(),
            id_kind: crate::config_store::IdKind::Serial,
            serial: None,
            mac: None,
            name: "N".into(),
            address: "h".into(),
            host: None,
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
            signal_poll_secs: Some(0),
            receiver_type: None,
            update_method: None,
            comm_api_version: None,
            has_tuner: None,
            tuner_presets_lettered: None,
            tuner: None,
            tuner_preset_names: Vec::new(),
            network_favorite_names: Vec::new(),
            info: None,
            missing: denon_marantz::MissingSources::default(),
        }
    }

    #[test]
    fn a_command_of_a_feature_that_the_receiver_does_not_list_is_not_offered() {
        use denon_marantz::{DeviceFeature, DeviceFeatures};
        let with = |features| {
            let mut c = crate::config_store::read_with_version(cfg(true, true), "0301");
            if let Some(info) = &mut c.info {
                info.features = features;
            }
            c
        };
        let lfe = DeviceFeatures::default().with(DeviceFeature::Lfe);
        assert!(supported(&with(None)).contains(&"PICTURE_MODE_MOVIE"));
        assert!(!supported(&with(Some(lfe))).contains(&"PICTURE_MODE_MOVIE"));
        // A command that needs no feature stays.
        assert!(supported(&with(Some(lfe))).contains(&"INFO_MENU"));
    }

    #[test]
    fn a_command_of_a_missing_quick_select_slot_is_not_offered() {
        assert_eq!(Some(2), quick_select_slot("QUICK_SELECT_MEMORY_2"));
        assert_eq!(Some(6), quick_select_slot("SMART_SELECT_6"));
        assert_eq!(None, quick_select_slot("ZONE_FAVORITE_1"));
        let c = crate::config_store::read_with_quick_selects(cfg(true, true), Some(3));
        let names = supported(&c);
        assert!(names.contains(&"QUICK_SELECT_3"));
        assert!(!names.contains(&"QUICK_SELECT_4"));
        assert!(!names.contains(&"QUICK_SELECT_MEMORY_4"));
    }

    #[test]
    fn the_counts_are_the_ones_of_the_python_integration_plus_older_receivers() {
        // Commands without a feature that the Python integration does not have.
        const NOT_PYTHON: [&str; 7] = [
            "BT_LEVEL_UP",
            "BT_LEVEL_DOWN",
            "TACTILE_CHANNEL_ON",
            "TACTILE_CHANNEL_OFF",
            "TACTILE_CHANNEL_TOGGLE",
            "TACTILE_CHANNEL_LEVEL_UP",
            "TACTILE_CHANNEL_LEVEL_DOWN",
        ];
        // The Python integration offers the Dirac commands on all receivers.
        let dirac = |is_denon| DeviceConfig {
            is_dirac_supported: Some(true),
            ..cfg(true, is_denon)
        };
        let python = |c: &DeviceConfig| {
            supported(c)
                .into_iter()
                .filter(|n| !NOT_PYTHON.contains(n))
                .filter(|n| {
                    get(n).is_some_and(|e| {
                        e.feature
                            .is_none_or(|f| matches!(f, Feature::Tuner | Feature::Dirac))
                            && spec_feature_of_line(e.brand_text(c.is_denon)).is_none()
                    })
                })
                .count()
        };
        assert_eq!(368, python(&dirac(true)));
        assert_eq!(380, python(&dirac(false)));
        assert_eq!(
            4 + supported(&cfg(true, true)).len(),
            supported(&dirac(true)).len()
        );
        // Before the receiver is read, and without `CommApiVers`, the commands of older receivers
        // are offered too.
        assert_eq!(430, supported(&cfg(true, true)).len());
        let missing = |is_denon| DeviceConfig {
            comm_api_version: Some(crate::config_store::CommApiVersion::MISSING),
            ..cfg(true, is_denon)
        };
        assert_eq!(430, supported(&missing(true)).len());
        assert_eq!(442, supported(&missing(false)).len());
        assert_eq!(ENTRIES.len(), 459);
        assert!(supported(&cfg(false, true)).len() < 430);
    }

    /// A toggle sends the opposite of the current value, so the driver must ask for that value
    /// after the connect: in the start groups of the library or in its own extra queries.
    #[test]
    fn the_value_of_each_toggle_is_asked_after_the_connect() {
        use crate::device::discovery::{EXTRA_QUERIES, OLDER_QUERIES};
        use denon_marantz::Brand;
        use denon_marantz::proto::telnet::{Query, QueryGroup, queries};
        let prefix = |query: &'static str| query.trim_end_matches('?').trim_end();
        let known_of = |brand| -> Vec<&str> {
            Query::ALL
                .iter()
                .filter_map(|query| query.text(brand))
                .map(prefix)
                .collect()
        };
        for (brand, other, other_brand) in [
            (Brand::Denon, "marantz", Brand::Marantz),
            (Brand::Marantz, "denon", Brand::Denon),
        ] {
            let driver = EXTRA_QUERIES
                .iter()
                .chain(OLDER_QUERIES.iter())
                .filter_map(|query| query.text(brand));
            let asked: Vec<&str> = queries(brand, QueryGroup::START_ORDER)
                .chain(driver)
                .map(prefix)
                .collect();
            let known = known_of(brand);
            let other_known = known_of(other_brand);
            for line in TABLE.lines() {
                let fields: Vec<&str> = line.split('\t').collect();
                let (name, text) = (fields[0], fields[3]);
                // A text with " / " has a Denon and a Marantz key code: it sends no value.
                if !name.ends_with("_TOGGLE")
                    || !text.contains('/')
                    || text.contains(" / ")
                    || fields[2] == other
                {
                    continue;
                }
                // The command part ends at the last space or `:` before the values, for example
                // `PSFH:` of `PSFH:ON/OFF`. A query must cover it: `MS?` does not cover `MSXYZ`.
                let values = text.find('/').unwrap_or(text.len());
                let head = text[..values]
                    .rfind([' ', ':'])
                    .map_or(&text[..values], |i| text[..=i].trim_end());
                let covers = |query: &str| text.starts_with(query) && query.len() >= head.len();
                // The query of the toggle is the longest one that covers it. The library has no
                // query for some toggles of the other brand.
                let Some(query) = known
                    .iter()
                    .filter(|query| covers(query))
                    .max_by_key(|query| query.len())
                else {
                    assert!(
                        other_known.iter().any(|query| covers(query)),
                        "{brand:?} {name}: the library has no query"
                    );
                    continue;
                };
                assert!(
                    asked.contains(query),
                    "{brand:?} {name}: {query} is not asked"
                );
            }
        }
    }

    #[test]
    fn every_feature_name_of_the_table_is_known() {
        for line in TABLE.lines() {
            if let Some(name) = line.split('\t').nth(4) {
                assert!(Feature::parse(name).is_some(), "{line}");
            }
        }
    }

    #[test]
    fn the_commands_of_older_receivers_follow_the_generation() {
        let with = |is_denon: bool, version: &str| {
            crate::config_store::read_with_version(cfg(true, is_denon), version)
        };
        let has = |c: &DeviceConfig, name: &str| supported(c).contains(&name);
        let denon_2013 = with(true, "0210");
        assert!(has(&denon_2013, "SURROUND_BACK_MODE_ON"));
        assert!(has(&denon_2013, "NETWORK_NEXT"));
        assert!(!has(&denon_2013, "DIALOG_LEVEL_UP"));
        assert!(!has(&denon_2013, "INPUT_MODE_7_1_IN"));
        assert!(!has(&denon_2013, "DYNAMIC_COMPRESSION_LOW"));
        let denon_2015 = with(true, "0300");
        assert!(has(&denon_2015, "DIALOG_LEVEL_UP"));
        assert!(!has(&denon_2015, "INSTA_PREVUE_ON"));
        assert!(!has(&denon_2015, "SURROUND_BACK_MODE_ON"));
        let marantz_2013 = with(false, "0210");
        assert!(has(&marantz_2013, "DYNAMIC_COMPRESSION_LOW"));
        assert!(has(&marantz_2013, "SOURCE_MENU_ON"));
        assert!(!has(&marantz_2013, "DIALOG_LEVEL_UP"));
        // No Marantz spec of 2014 to 2016: all are offered.
        assert!(has(&with(false, "0300"), "DIALOG_LEVEL_UP"));
        let denon_now = with(true, "0301");
        assert!(!has(&denon_now, "PRO_LOGIC_MODE_CINEMA"));
        assert!(!has(&denon_now, "FRONT_HEIGHT_ON"));
        assert!(!has(&denon_now, "NETWORK_NEXT"));
        assert!(has(&denon_now, "INPUT_MODE_7_1_IN"));
        assert!(has(&denon_now, "INPUT_MODE_SELECT"));
        assert!(!has(&with(false, "0301"), "FRONT_HEIGHT_ON"));
        assert!(!has(&with(false, "0301"), "AUTO_FLAG_DETECT_ON"));
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
        assert_eq!("PSFH:OFF", toggled("PSFH:ON/OFF", Some("ON")));
        assert_eq!(
            "PSCINEMA EQ.OFF",
            toggled("PSCINEMA EQ.ON/EQ.OFF", Some("EQ.ON"))
        );
        let state = State::new(true);
        assert_eq!(
            "BTTX ON",
            get("BT_TRANSMITTER_TOGGLE")
                .expect("cmd")
                .text(true, &state)
        );
    }
}
