// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Select entities.

use crate::config_store::DeviceConfig;
use crate::entities::sensor::EMPTY;
use crate::entities::{SelectKind, View, availability, is_off};
use crate::errors::ServiceError;
use denon_marantz::proto::source::{display_to_select_code, telnet_to_display_source};
use denon_marantz::proto::{
    BluetoothOutputMode, DigitalCodec, DimmerMode, DiracFilter, DiracSlot, DynamicVolume, EcoMode,
    HdmiOutput, MultiEq, PictureMode, ReferenceLevelOffset, SoundModeGroup, SpeakerPreset,
    TunerBand,
};
use denon_marantz::{Avr, SoundModeEntry, State, Zone};
use serde_json::{Map, Value};

/// Options of a select: the shown text, and the current one.
pub struct Choices {
    pub options: Vec<String>,
    pub current: Option<String>,
    /// The options are numbers, as the Python integration sends them.
    pub numeric: bool,
}

/// The option of the active sound mode. The receiver reports its own text for it, for example
/// `MULTI CH IN` for the list entry `Multi Ch In`. `None` if no option fits.
pub fn active_sound_mode(state: &State) -> Option<&str> {
    let raw = state.zone(Zone::Main).sound_mode.as_deref()?;
    let modes = &state.device().sound_modes;
    if modes.as_slice().is_empty() {
        // Without the list, the options are the groups.
        return SoundModeGroup::from_raw(raw)
            .or_else(|| SoundModeGroup::guess_from_raw(raw))
            .map(|group| group_option(group).code());
    }
    modes.entry_for(raw).map(|entry| entry.name.as_str())
}

/// The group option that shows a group. As in the Python integration, Multi Ch In is part of the
/// movie group: it has no option of its own.
fn group_option(group: SoundModeGroup) -> SoundModeGroup {
    if group == SoundModeGroup::MultiChIn {
        SoundModeGroup::Movie
    } else {
        group
    }
}

/// Option order of the Python integration.
const ECO_MODES: &[EcoMode] = &[EcoMode::On, EcoMode::Auto, EcoMode::Off];
const DIRAC_FILTERS: &[DiracFilter] = &[
    DiracFilter::Slot1,
    DiracFilter::Slot2,
    DiracFilter::Slot3,
    DiracFilter::Off,
];

pub const fn name(kind: SelectKind) -> &'static str {
    match kind {
        SelectKind::SoundMode => "Sound Mode",
        SelectKind::InputSource => "Input Source",
        SelectKind::ReferenceLevelOffset => "Reference Level",
        SelectKind::DynamicVolume => "Dynamic Volume",
        SelectKind::Dimmer => "Dimmer",
        SelectKind::EcoMode => "Eco Mode",
        SelectKind::MonitorOutput => "Monitor Output",
        SelectKind::SpeakerPreset => "Speaker Preset",
        SelectKind::PictureMode => "Picture Mode",
        SelectKind::DigitalCodec => "Digital Codec",
        SelectKind::DiracFilter => "Dirac Filter",
        SelectKind::MultiEq => "Audyssey MultEQ",
        SelectKind::QuickSelect => "Quick Select",
        SelectKind::TunerBand => "Tuner Band",
        SelectKind::BluetoothOutput => "Bluetooth Output",
    }
}

/// Mode of the Audyssey `MultEQ` that can be selected. `Manual` is only reported.
const MULTI_EQ_MODES: &[MultiEq] = &[
    MultiEq::Off,
    MultiEq::Flat,
    MultiEq::LrBypass,
    MultiEq::Reference,
];
const TUNER_BANDS: &[TunerBand] = &[TunerBand::Am, TunerBand::Fm];
/// Quick selects of a receiver: the first five exist on all models.
const QUICK_SELECTS: usize = 5;

/// Six selects always exist. With a telnet connection there are more, and the Dirac filter if the
/// receiver supports it. Over HTTP, the `MultEq` select also comes with `AppCommand` if the receiver
/// lists the query, and the monitor output if the receiver reads it over HTTP.
pub const fn available(cfg: &DeviceConfig, kind: SelectKind) -> bool {
    match kind {
        SelectKind::SoundMode
        | SelectKind::InputSource
        | SelectKind::ReferenceLevelOffset
        | SelectKind::DynamicVolume
        | SelectKind::Dimmer
        | SelectKind::EcoMode => true,
        SelectKind::MonitorOutput => cfg.use_telnet || cfg.reads_hdmi_output(),
        SelectKind::QuickSelect => cfg.use_telnet && cfg.has_quick_select(1),
        SelectKind::SpeakerPreset
        | SelectKind::PictureMode
        | SelectKind::DigitalCodec
        | SelectKind::TunerBand
        | SelectKind::BluetoothOutput => cfg.use_telnet,
        SelectKind::MultiEq => cfg.use_telnet || cfg.reads_audyssey(),
        SelectKind::DiracFilter => cfg.use_telnet && cfg.has_dirac(),
    }
}

/// Insert the attributes of a select.
pub fn attributes(v: &View<'_>, kind: SelectKind, out: &mut Map<String, Value>) {
    out.insert("state".into(), availability(v).into());
    let choices = choices(v, kind);
    // While the receiver is off, only the input source keeps its options.
    let (options, current): (Vec<String>, String) = if is_off(v) {
        let options = if kind == SelectKind::InputSource {
            choices.options
        } else {
            vec![EMPTY.to_owned()]
        };
        (options, EMPTY.to_owned())
    } else {
        (
            choices.options,
            choices.current.unwrap_or_else(|| EMPTY.to_owned()),
        )
    };
    let numeric = choices.numeric && !is_off(v);
    let number = |text: &str| text.parse::<u8>().map_or_else(|_| text.into(), Value::from);
    if numeric {
        out.insert(
            "options".into(),
            options.iter().map(|o| number(o)).collect::<Vec<_>>().into(),
        );
        out.insert("current_option".into(), number(&current));
    } else {
        out.insert("options".into(), options.into());
        out.insert("current_option".into(), current.into());
    }
}

/// All options of the select and the current one.
#[allow(clippy::too_many_lines)]
pub fn choices(v: &View<'_>, kind: SelectKind) -> Choices {
    let zone = v.state.zone(Zone::Main);
    let device = v.state.device();
    match kind {
        SelectKind::SoundMode => {
            let entries = device.sound_modes.as_slice();
            let mut seen = std::collections::HashSet::with_capacity(entries.len());
            let mut options: Vec<String> = Vec::with_capacity(entries.len());
            for entry in entries {
                if seen.insert(entry.name.as_str()) {
                    options.push(entry.name.clone());
                }
            }
            // Without telnet the receiver gives no list: the groups are the choices.
            if options.is_empty() {
                options.extend(
                    SoundModeGroup::ALL
                        .iter()
                        .filter(|g| **g == group_option(**g))
                        .map(|g| g.code().to_owned()),
                );
            }
            // The receiver reports its own text for the active mode: `active_sound_mode` maps it to an
            // option.
            Choices {
                options,
                current: v.sound_mode.map(str::to_owned),
                numeric: false,
            }
        }
        SelectKind::InputSource => {
            let current = zone.source.as_deref().map(|source| {
                let code = if device.sources.get(source).is_some() {
                    source
                } else {
                    display_to_select_code(source).unwrap_or(source)
                };
                source_label(v, code)
            });
            Choices {
                options: source_options(v)
                    .map(|(_, label)| label.to_owned())
                    .collect(),
                current,
                numeric: false,
            }
        }
        SelectKind::DiracFilter => {
            let labels = dirac_labels(v);
            let position = device
                .dirac_filter
                .and_then(|filter| DIRAC_FILTERS.iter().position(|f| *f == filter));
            let slots = &device.dirac_slots;
            // Only slots with a filter can be selected. The active one is always shown.
            let options = labels
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    DIRAC_FILTERS[*i] == DiracFilter::Off
                        || slot_has_filter(slots.get(*i))
                        || position == Some(*i)
                })
                .map(|(_, label)| label.clone())
                .collect();
            Choices {
                options,
                current: position.map(|i| labels[i].clone()),
                numeric: false,
            }
        }
        SelectKind::SpeakerPreset => {
            let mut c = enum_choices(SpeakerPreset::ALL, device.speaker_preset, |x| match x {
                SpeakerPreset::One => "1",
                _ => "2",
            });
            c.numeric = true;
            c
        }
        SelectKind::ReferenceLevelOffset => enum_choices(
            ReferenceLevelOffset::ALL,
            device.reference_level_offset,
            ReferenceLevelOffset::label,
        ),
        SelectKind::DynamicVolume => enum_choices(
            DynamicVolume::ALL,
            device.dynamic_volume,
            DynamicVolume::label,
        ),
        SelectKind::Dimmer => enum_choices(DimmerMode::ALL, device.dimmer, DimmerMode::label),
        SelectKind::EcoMode => enum_choices(ECO_MODES, device.eco_mode, EcoMode::label),
        SelectKind::MonitorOutput => enum_choices(
            HdmiOutput::ALL,
            device.video_settings.hdmi_output,
            HdmiOutput::label,
        ),
        SelectKind::PictureMode => enum_choices(
            PictureMode::ALL,
            device.picture_settings.mode,
            PictureMode::label,
        ),
        SelectKind::DigitalCodec => {
            enum_choices(DigitalCodec::ALL, device.digital_codec, DigitalCodec::label)
        }
        SelectKind::MultiEq => {
            let mut c = enum_choices(MULTI_EQ_MODES, device.multi_eq, MultiEq::label);
            // `Manual` cannot be selected, but it is shown while it is active.
            if device.multi_eq == Some(MultiEq::Manual) {
                c.options.push(MultiEq::Manual.label().to_owned());
            }
            c
        }
        SelectKind::TunerBand => enum_choices(TUNER_BANDS, device.tuner.band, TunerBand::label),
        SelectKind::BluetoothOutput => enum_choices(
            BluetoothOutputMode::ALL,
            device.bluetooth_output,
            BluetoothOutputMode::label,
        ),
        SelectKind::QuickSelect => {
            let labels = quick_select_labels(v);
            let current = zone
                .quick_select
                .and_then(|n| labels.get(usize::from(n).checked_sub(1)?).cloned());
            Choices {
                options: labels,
                current,
                numeric: false,
            }
        }
    }
}

/// The number of the quick select with this shown name, from 1.
fn quick_select_number(labels: &[String], option: &str) -> Option<u8> {
    let index = labels.iter().position(|label| label == option)?;
    u8::try_from(index + 1).ok()
}

/// The shown names of the quick selects: the name that the user gave it, else `Quick Select n`
/// (`Smart Select n` on a Marantz receiver).
fn quick_select_labels(v: &View<'_>) -> Vec<String> {
    let names = &v.state.device().quick_select_names;
    let kind = if v.cfg.is_denon { "Quick" } else { "Smart" };
    let count = v.cfg.quick_select_count.map_or_else(
        || names.len().clamp(QUICK_SELECTS, 6),
        |n| usize::from(n).min(6),
    );
    (0..count)
        .map(|i| {
            names
                .get(i)
                .and_then(Option::as_deref)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map_or_else(|| format!("{kind} Select {}", i + 1), str::to_owned)
        })
        .collect()
}

fn enum_choices<T: Copy>(all: &[T], current: Option<T>, label: fn(T) -> &'static str) -> Choices {
    Choices {
        options: all.iter().map(|v| label(*v).to_owned()).collect(),
        current: current.map(|v| label(v).to_owned()),
        numeric: false,
    }
}

/// A slot has a filter that can be selected. A slot with unknown data counts as usable.
fn slot_has_filter(slot: Option<&DiracSlot>) -> bool {
    slot.is_none_or(|s| {
        s.imported != Some(false)
            && !s
                .kind
                .as_deref()
                .is_some_and(|k| k.eq_ignore_ascii_case("NSA"))
    })
}

/// The options of the Dirac filter, in the order of `DIRAC_FILTERS`. A slot shows the name that the
/// filter was saved with, else `Slot 1` to `Slot 3` as the Python integration. The same name in
/// two slots gets the slot number.
fn dirac_labels(v: &View<'_>) -> Vec<String> {
    let slots = &v.state.device().dirac_slots;
    let mut labels: Vec<String> = DIRAC_FILTERS
        .iter()
        .enumerate()
        .map(|(i, filter)| {
            slots
                .get(i)
                .and_then(|slot| slot.name.as_deref())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map_or_else(|| filter.label().to_owned(), str::to_owned)
        })
        .collect();
    let names = labels.clone();
    for (i, label) in labels.iter_mut().enumerate() {
        if names.iter().filter(|n| **n == names[i]).count() > 1 {
            *label = format!("{} ({})", names[i], DIRAC_FILTERS[i].label());
        }
    }
    labels
}

/// Sources to show: the telnet code and the shown name.
pub fn source_options<'a>(v: &'a View<'_>) -> impl Iterator<Item = (&'a str, &'a str)> {
    v.state
        .device()
        .sources
        .as_slice()
        .iter()
        .filter(|s| v.cfg.show_all_inputs || s.in_use != Some(false))
        .map(|s| {
            let label = s
                .name
                .as_deref()
                .or(s.default_name.as_deref())
                .unwrap_or(&s.code);
            (s.code.as_str(), label)
        })
}

/// Shown name of a source code.
pub fn source_label(v: &View<'_>, code: &str) -> String {
    let sources = &v.state.device().sources;
    sources
        .get(code)
        .and_then(|s| s.name.as_deref().or(s.default_name.as_deref()))
        .unwrap_or_else(|| telnet_to_display_source(code))
        .to_owned()
}

enum SoundModeChoice<'a> {
    Entry(&'a SoundModeEntry),
    Group(SoundModeGroup),
}

/// A name from the receiver list first, then a group name. The Remote can send a group name from
/// the list it got before the receiver list arrived.
fn sound_mode_choice<'a>(modes: &'a [SoundModeEntry], option: &str) -> Option<SoundModeChoice<'a>> {
    modes
        .iter()
        .find(|e| e.name == option)
        .map(SoundModeChoice::Entry)
        .or_else(|| {
            SoundModeGroup::ALL
                .iter()
                .find(|g| g.code().eq_ignore_ascii_case(option))
                .map(|g| SoundModeChoice::Group(*g))
        })
}

fn find<T: Copy>(all: &[T], label: fn(T) -> &'static str, option: &str) -> Option<T> {
    all.iter().copied().find(|v| label(*v) == option)
}

/// Select an option. Setters return when the line is written.
#[allow(clippy::too_many_lines)]
pub async fn select_option(
    avr: &Avr,
    v: &View<'_>,
    kind: SelectKind,
    option: &str,
) -> Result<(), ServiceError> {
    let invalid = || ServiceError::BadRequest(format!("Unknown option: {option}"));
    match kind {
        SelectKind::SoundMode => {
            let main = avr.zone(Zone::Main)?;
            let modes = v.state.device().sound_modes.as_slice();
            match sound_mode_choice(modes, option).ok_or_else(invalid)? {
                SoundModeChoice::Entry(entry) => main.select_sound_mode_entry(entry).await?,
                SoundModeChoice::Group(group) => main.select_sound_mode(group.code()).await?,
            }
        }
        SelectKind::InputSource => {
            let (code, _) = source_options(v)
                .find(|(_, label)| *label == option)
                .ok_or_else(invalid)?;
            avr.zone(Zone::Main)?.select_source(code).await?;
        }
        SelectKind::DiracFilter => {
            // The shown name, or the name of the Python integration (`Slot 2`).
            let labels = dirac_labels(v);
            let option = option.trim();
            let filter = DIRAC_FILTERS
                .iter()
                .zip(&labels)
                .find(|(f, label)| label.eq_ignore_ascii_case(option) || f.label() == option)
                .map(|(f, _)| *f)
                .ok_or_else(invalid)?;
            avr.dirac()?.set_filter(filter).await?;
        }
        SelectKind::SpeakerPreset => {
            let x = match option.trim() {
                "1" => SpeakerPreset::One,
                "2" => SpeakerPreset::Two,
                _ => return Err(invalid()),
            };
            avr.sound().set_speaker_preset(x).await?;
        }
        SelectKind::ReferenceLevelOffset => {
            let x = find(
                ReferenceLevelOffset::ALL,
                ReferenceLevelOffset::label,
                option,
            )
            .ok_or_else(invalid)?;
            avr.audyssey().set_reference_level_offset(x).await?;
        }
        SelectKind::DynamicVolume => {
            let x = find(DynamicVolume::ALL, DynamicVolume::label, option).ok_or_else(invalid)?;
            avr.audyssey().set_dynamic_volume(x).await?;
        }
        SelectKind::Dimmer => {
            let x = find(DimmerMode::ALL, DimmerMode::label, option).ok_or_else(invalid)?;
            avr.device().set_dimmer(x).await?;
        }
        SelectKind::EcoMode => {
            let x = find(ECO_MODES, EcoMode::label, option).ok_or_else(invalid)?;
            avr.device().set_eco_mode(x).await?;
        }
        SelectKind::MonitorOutput => {
            let x = find(HdmiOutput::ALL, HdmiOutput::label, option).ok_or_else(invalid)?;
            avr.video().set_hdmi_output(x).await?;
        }
        SelectKind::PictureMode => {
            let x = find(PictureMode::ALL, PictureMode::label, option).ok_or_else(invalid)?;
            avr.video().set_picture_mode(x).await?;
        }
        SelectKind::DigitalCodec => {
            let x = find(DigitalCodec::ALL, DigitalCodec::label, option).ok_or_else(invalid)?;
            avr.device().set_digital_codec(x).await?;
        }
        SelectKind::MultiEq => {
            let x = find(MULTI_EQ_MODES, MultiEq::label, option).ok_or_else(invalid)?;
            avr.audyssey().set_multi_eq(x).await?;
        }
        SelectKind::TunerBand => {
            let x = find(TUNER_BANDS, TunerBand::label, option).ok_or_else(invalid)?;
            avr.tuner().set_band(x).await?;
        }
        SelectKind::BluetoothOutput => {
            let x = find(BluetoothOutputMode::ALL, BluetoothOutputMode::label, option)
                .ok_or_else(invalid)?;
            avr.device().set_bluetooth_output(x).await?;
        }
        SelectKind::QuickSelect => {
            let number =
                quick_select_number(&quick_select_labels(v), option).ok_or_else(invalid)?;
            avr.zone(Zone::Main)?.select_quick_select(number).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Vec<String> {
        ["Quick Select 1", "Movie night", "Quick Select 3"]
            .map(str::to_owned)
            .to_vec()
    }

    #[test]
    fn a_quick_select_is_found_by_its_shown_name() {
        assert_eq!(Some(1), quick_select_number(&labels(), "Quick Select 1"));
        assert_eq!(Some(2), quick_select_number(&labels(), "Movie night"));
        assert_eq!(Some(3), quick_select_number(&labels(), "Quick Select 3"));
    }

    #[test]
    fn an_unknown_quick_select_is_refused() {
        assert_eq!(None, quick_select_number(&labels(), "Quick Select 2"));
        assert_eq!(None, quick_select_number(&labels(), ""));
    }

    fn entry(name: &str) -> SoundModeEntry {
        SoundModeEntry {
            category: denon_marantz::proto::SoundModeCategory::Movie,
            index: 1,
            name: name.to_owned(),
        }
    }

    #[test]
    fn a_sound_mode_is_found_by_receiver_name_or_group_name() {
        let modes = [entry("Dolby Surround"), entry("Stereo")];
        assert!(matches!(
            sound_mode_choice(&modes, "Stereo"),
            Some(SoundModeChoice::Entry(e)) if e.name == "Stereo"
        ));
        // A group name from the list sent before the receiver list.
        assert!(matches!(
            sound_mode_choice(&modes, "MCH STEREO"),
            Some(SoundModeChoice::Group(SoundModeGroup::McHStereo))
        ));
        assert!(matches!(
            sound_mode_choice(&[], "stereo"),
            Some(SoundModeChoice::Group(SoundModeGroup::Stereo))
        ));
        assert!(sound_mode_choice(&modes, "Nonsense").is_none());
    }

    #[test]
    fn the_audyssey_mode_options_can_be_selected() {
        for label in ["Off", "Flat", "L/R Bypass", "Reference"] {
            assert!(
                find(MULTI_EQ_MODES, MultiEq::label, label).is_some(),
                "{label}"
            );
        }
        // `Manual` is only reported by the receiver.
        assert!(find(MULTI_EQ_MODES, MultiEq::label, "Manual").is_none());
    }

    #[test]
    fn the_tuner_and_bluetooth_options_can_be_selected() {
        assert_eq!(
            Some(TunerBand::Fm),
            find(TUNER_BANDS, TunerBand::label, "FM")
        );
        assert!(find(TUNER_BANDS, TunerBand::label, "DAB").is_none());
        assert_eq!(
            Some(BluetoothOutputMode::BluetoothOnly),
            find(
                BluetoothOutputMode::ALL,
                BluetoothOutputMode::label,
                "Bluetooth Only"
            )
        );
    }
}
