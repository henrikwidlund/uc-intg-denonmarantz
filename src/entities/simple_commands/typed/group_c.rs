// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Typed calls of simple commands.

use super::Run;
use denon_marantz::proto::{
    Channel, HdmiOutput, ImaxMode, MultiEq, PanelLock, PictureMode, ReferenceLevelOffset,
    RemoteKey, RoomSize, Sleep, SpeakerPreset, Zone,
};

pub const TABLE: &[(&str, Run)] = &[
    call!("MULTIEQ_OFF", |avr, cx| avr
        .audyssey()
        .set_multi_eq(MultiEq::Off)),
    call!("MULTIEQ_REFERENCE", |avr, cx| avr
        .audyssey()
        .set_multi_eq(MultiEq::Reference)),
    call!("NETWORK_RESTART", |avr, cx| avr.device().restart_network()),
    call!("OUTPUT_1", |avr, cx| avr
        .video()
        .set_hdmi_output(HdmiOutput::Hdmi1)),
    call!("OUTPUT_2", |avr, cx| avr
        .video()
        .set_hdmi_output(HdmiOutput::Hdmi2)),
    call!("OUTPUT_AUTO", |avr, cx| avr
        .video()
        .set_hdmi_output(HdmiOutput::Auto)),
    call!("PAGE_DOWN", |avr, cx| avr
        .zone(Zone::Main)?
        .press(RemoteKey::PageDown)),
    call!("PAGE_UP", |avr, cx| avr
        .zone(Zone::Main)?
        .press(RemoteKey::PageUp)),
    call!("PANEL_LOCK_OFF", |avr, cx| avr
        .device()
        .set_panel_lock(PanelLock::Off)),
    call!("PANEL_LOCK_PANEL", |avr, cx| avr
        .device()
        .set_panel_lock(PanelLock::Panel)),
    call!("PANEL_LOCK_PANEL_VOLUME", |avr, cx| avr
        .device()
        .set_panel_lock(PanelLock::PanelAndVolume)),
    call!("PANORAMA_OFF", |avr, cx| avr.sound().set_panorama(false)),
    call!("PANORAMA_ON", |avr, cx| avr.sound().set_panorama(true)),
    call!("PICTURE_MODE_BRILLIANT", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::Brilliant)),
    call!("PICTURE_MODE_CUSTOM", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::Custom)),
    call!("PICTURE_MODE_GAME", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::Game)),
    call!("PICTURE_MODE_ISF_DAY", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::IsfDay)),
    call!("PICTURE_MODE_ISF_NIGHT", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::IsfNight)),
    call!("PICTURE_MODE_MOVIE", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::Movie)),
    call!("PICTURE_MODE_OFF", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::Off)),
    call!("PICTURE_MODE_STREAM", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::Stream)),
    call!("PICTURE_MODE_VIVID", |avr, cx| avr
        .video()
        .set_picture_mode(PictureMode::Vivid)),
    call!("QUICK_SELECT_1", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(1)),
    call!("QUICK_SELECT_2", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(2)),
    call!("QUICK_SELECT_3", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(3)),
    call!("QUICK_SELECT_4", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(4)),
    call!("QUICK_SELECT_5", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(5)),
    call!("QUICK_SELECT_6", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(6)),
    call!("QUICK_SELECT_MEMORY_1", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(1)),
    call!("QUICK_SELECT_MEMORY_2", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(2)),
    call!("QUICK_SELECT_MEMORY_3", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(3)),
    call!("QUICK_SELECT_MEMORY_4", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(4)),
    call!("QUICK_SELECT_MEMORY_5", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(5)),
    call!("QUICK_SELECT_MEMORY_6", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(6)),
    call!("REAR_HEIGHT_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::RearHeightLeft)),
    call!("REAR_HEIGHT_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::RearHeightLeft)),
    call!("REAR_HEIGHT_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::RearHeightRight)),
    call!("REAR_HEIGHT_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::RearHeightRight)),
    call!("REFERENCE_LEVEL_OFFSET_0DB", |avr, cx| avr
        .audyssey()
        .set_reference_level_offset(ReferenceLevelOffset::Db0)),
    call!("REFERENCE_LEVEL_OFFSET_10DB", |avr, cx| avr
        .audyssey()
        .set_reference_level_offset(ReferenceLevelOffset::Db10)),
    call!("REFERENCE_LEVEL_OFFSET_15DB", |avr, cx| avr
        .audyssey()
        .set_reference_level_offset(ReferenceLevelOffset::Db15)),
    call!("REFERENCE_LEVEL_OFFSET_5DB", |avr, cx| avr
        .audyssey()
        .set_reference_level_offset(ReferenceLevelOffset::Db5)),
    call!("REMOTE_CONTROL_LOCK_OFF", |avr, cx| avr
        .device()
        .set_remote_lock(false)),
    call!("REMOTE_CONTROL_LOCK_ON", |avr, cx| avr
        .device()
        .set_remote_lock(true)),
    call!("ROOM_SIZE_LARGE", |avr, cx| avr
        .sound()
        .set_room_size(RoomSize::Large)),
    call!("ROOM_SIZE_MEDIUM", |avr, cx| avr
        .sound()
        .set_room_size(RoomSize::Medium)),
    call!("ROOM_SIZE_MEDIUM_LARGE", |avr, cx| avr
        .sound()
        .set_room_size(RoomSize::MediumLarge)),
    call!("ROOM_SIZE_MEDIUM_SMALL", |avr, cx| avr
        .sound()
        .set_room_size(RoomSize::MediumSmall)),
    call!("ROOM_SIZE_SMALL", |avr, cx| avr
        .sound()
        .set_room_size(RoomSize::Small)),
    call!("SLEEP_TIMER_120MIN", |avr, cx| avr
        .zone(Zone::Main)?
        .set_sleep(Sleep::Minutes(120))),
    call!("SLEEP_TIMER_30MIN", |avr, cx| avr
        .zone(Zone::Main)?
        .set_sleep(Sleep::Minutes(30))),
    call!("SLEEP_TIMER_60MIN", |avr, cx| avr
        .zone(Zone::Main)?
        .set_sleep(Sleep::Minutes(60))),
    call!("SLEEP_TIMER_90MIN", |avr, cx| avr
        .zone(Zone::Main)?
        .set_sleep(Sleep::Minutes(90))),
    call!("SLEEP_TIMER_OFF", |avr, cx| avr
        .zone(Zone::Main)?
        .set_sleep(Sleep::Off)),
    call!("SMART_SELECT_1", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(1)),
    call!("SMART_SELECT_2", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(2)),
    call!("SMART_SELECT_3", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(3)),
    call!("SMART_SELECT_4", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(4)),
    call!("SMART_SELECT_5", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(5)),
    call!("SMART_SELECT_6", |avr, cx| avr
        .zone(Zone::Main)?
        .select_quick_select(6)),
    call!("SMART_SELECT_MEMORY_1", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(1)),
    call!("SMART_SELECT_MEMORY_2", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(2)),
    call!("SMART_SELECT_MEMORY_3", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(3)),
    call!("SMART_SELECT_MEMORY_4", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(4)),
    call!("SMART_SELECT_MEMORY_5", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(5)),
    call!("SMART_SELECT_MEMORY_6", |avr, cx| avr
        .zone(Zone::Main)?
        .store_quick_select(6)),
    call!("SOUND_MODE_IMAX_AUTO", |avr, cx| avr
        .sound()
        .set_imax(ImaxMode::Auto)),
    call!("SOUND_MODE_IMAX_OFF", |avr, cx| avr
        .sound()
        .set_imax(ImaxMode::Off)),
    call!("SOUND_MODE_IMAX_TOGGLE", |avr, cx| avr.sound().set_imax(
        match cx.state.device().sound_settings.imax {
            Some(ImaxMode::Off) | None => ImaxMode::Auto,
            Some(_) => ImaxMode::Off,
        }
    )),
    call!("SOUND_MODE_NEURAL_X_OFF", |avr, cx| avr
        .sound()
        .set_neural_x(false)),
    call!("SOUND_MODE_NEURAL_X_ON", |avr, cx| avr
        .sound()
        .set_neural_x(true)),
    call!("SOUND_MODE_NEURAL_X_TOGGLE", |avr, cx| avr
        .sound()
        .set_neural_x(
            cx.state.device().sound_settings.neural_x != Some(true)
        )),
    call!("SPEAKER_PRESET_1", |avr, cx| avr
        .sound()
        .set_speaker_preset(SpeakerPreset::One)),
    call!("SPEAKER_PRESET_2", |avr, cx| avr
        .sound()
        .set_speaker_preset(SpeakerPreset::Two)),
    call!("SPEAKER_PRESET_TOGGLE", |avr, cx| avr
        .sound()
        .set_speaker_preset(match cx.state.device().speaker_preset {
            Some(SpeakerPreset::One) => SpeakerPreset::Two,
            _ => SpeakerPreset::One,
        })),
    call!("SPEAKER_VIRTUALIZER_OFF", |avr, cx| avr
        .sound()
        .set_speaker_virtualizer(false)),
    call!("SPEAKER_VIRTUALIZER_ON", |avr, cx| avr
        .sound()
        .set_speaker_virtualizer(true)),
    call!("SPEAKER_VIRTUALIZER_TOGGLE", |avr, cx| avr
        .sound()
        .set_speaker_virtualizer(
            cx.state.device().sound_settings.speaker_virtualizer != Some(true)
        )),
    call!("STATUS", |avr, cx| avr.device().show_status()),
    call!("SUB1_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::Subwoofer)),
    call!("SUB1_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::Subwoofer)),
    call!("SUB2_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::Subwoofer2)),
    call!("SUB2_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::Subwoofer2)),
    call!("SUB3_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::Subwoofer3)),
    call!("SUB3_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::Subwoofer3)),
    call!("SUB4_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::Subwoofer4)),
    call!("SUB4_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::Subwoofer4)),
    call!("SUBWOOFER1_LEVEL_DOWN", |avr, cx| avr
        .sound()
        .subwoofer_level_down(1)),
    call!("SUBWOOFER1_LEVEL_UP", |avr, cx| avr
        .sound()
        .subwoofer_level_up(1)),
    call!("SUBWOOFER2_LEVEL_DOWN", |avr, cx| avr
        .sound()
        .subwoofer_level_down(2)),
    call!("SUBWOOFER2_LEVEL_UP", |avr, cx| avr
        .sound()
        .subwoofer_level_up(2)),
    call!("SUBWOOFER3_LEVEL_DOWN", |avr, cx| avr
        .sound()
        .subwoofer_level_down(3)),
    call!("SUBWOOFER3_LEVEL_UP", |avr, cx| avr
        .sound()
        .subwoofer_level_up(3)),
    call!("SUBWOOFER4_LEVEL_DOWN", |avr, cx| avr
        .sound()
        .subwoofer_level_down(4)),
    call!("SUBWOOFER4_LEVEL_UP", |avr, cx| avr
        .sound()
        .subwoofer_level_up(4)),
];
