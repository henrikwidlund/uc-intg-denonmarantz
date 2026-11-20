// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Typed calls of simple commands.

use super::Run;
use denon_marantz::Command;
use denon_marantz::proto::{
    AudioRestorer, Auro3dMode, AuroMatic3dPreset, AutoStandby, BluetoothOutputMode, Channel,
    DacFilter, DialogEnhancerLevel, DigitalCodec, DimmerMode, DiracFilter, Drc, DynamicVolume,
    EcoMode, EffectSpeakerSelection, Zone,
};

pub const TABLE: &[(&str, Run)] = &[
    call!("DIMMER_BRIGHT", |avr, cx| avr
        .device()
        .set_dimmer(DimmerMode::Bright)),
    call!("DIMMER_DIM", |avr, cx| avr
        .device()
        .set_dimmer(DimmerMode::Dim)),
    call!("DIMMER_DARK", |avr, cx| avr
        .device()
        .set_dimmer(DimmerMode::Dark)),
    call!("DIMMER_OFF", |avr, cx| avr
        .device()
        .set_dimmer(DimmerMode::Off)),
    call!("DIMMER_TOGGLE", |avr, cx| avr.device().dimmer_next()),
    call!("ECO_ON", |avr, cx| avr.device().set_eco_mode(EcoMode::On)),
    call!("ECO_AUTO", |avr, cx| avr
        .device()
        .set_eco_mode(EcoMode::Auto)),
    call!("ECO_OFF", |avr, cx| avr.device().set_eco_mode(EcoMode::Off)),
    call!("ALL_ZONE_STEREO_OFF", |avr, cx| avr
        .send(Zone::Main, Command::AllZoneStereo(false))),
    call!("ALL_ZONE_STEREO_ON", |avr, cx| avr
        .send(Zone::Main, Command::AllZoneStereo(true))),
    call!("AUDIO_RESTORER_HIGH", |avr, cx| avr
        .sound()
        .set_audio_restorer(AudioRestorer::High)),
    call!("AUDIO_RESTORER_LOW", |avr, cx| avr
        .sound()
        .set_audio_restorer(AudioRestorer::Low)),
    call!("AUDIO_RESTORER_MEDIUM", |avr, cx| avr
        .sound()
        .set_audio_restorer(AudioRestorer::Medium)),
    call!("AUDIO_RESTORER_OFF", |avr, cx| avr
        .sound()
        .set_audio_restorer(AudioRestorer::Off)),
    call!("AUDYSSEY_LFC", |avr, cx| avr.audyssey().set_lfc(true)),
    call!("AUDYSSEY_LFC_OFF", |avr, cx| avr.audyssey().set_lfc(false)),
    call!("AUDYSSEY_LFC_TOGGLE", |avr, cx| avr
        .audyssey()
        .set_lfc(cx.state.device().lfc != Some(true))),
    call!("AUROMATIC_3D_PRESET_LARGE", |avr, cx| avr
        .sound()
        .set_auro_matic_preset(AuroMatic3dPreset::Large)),
    call!("AUROMATIC_3D_PRESET_MEDIUM", |avr, cx| avr
        .sound()
        .set_auro_matic_preset(AuroMatic3dPreset::Medium)),
    call!("AUROMATIC_3D_PRESET_MOVIE", |avr, cx| avr
        .sound()
        .set_auro_matic_preset(AuroMatic3dPreset::Movie)),
    call!("AUROMATIC_3D_PRESET_SMALL", |avr, cx| avr
        .sound()
        .set_auro_matic_preset(AuroMatic3dPreset::Small)),
    call!("AUROMATIC_3D_PRESET_SPEECH", |avr, cx| avr
        .sound()
        .set_auro_matic_preset(AuroMatic3dPreset::Speech)),
    call!("AUROMATIC_3D_STRENGTH_DOWN", |avr, cx| avr
        .sound()
        .auro_matic_strength_down()),
    call!("AUROMATIC_3D_STRENGTH_UP", |avr, cx| avr
        .sound()
        .auro_matic_strength_up()),
    call!("AURO_3D_MODE_CHANNEL_EXPANSION", |avr, cx| avr
        .sound()
        .set_auro_3d_mode(Auro3dMode::ChannelExpansion)),
    call!("AURO_3D_MODE_DIRECT", |avr, cx| avr
        .sound()
        .set_auro_3d_mode(Auro3dMode::Direct)),
    call!("AUTO_LIP_SYNC_OFF", |avr, cx| avr
        .send(Zone::Main, Command::AutoLipSync(false))),
    call!("AUTO_LIP_SYNC_ON", |avr, cx| avr
        .send(Zone::Main, Command::AutoLipSync(true))),
    call!("AUTO_STANDBY_15MIN", |avr, cx| async move {
        avr.zone(Zone::Main)?
            .set_auto_standby(AutoStandby::Minutes15)
            .await
    }),
    call!("AUTO_STANDBY_30MIN", |avr, cx| async move {
        avr.zone(Zone::Main)?
            .set_auto_standby(AutoStandby::Minutes30)
            .await
    }),
    call!("AUTO_STANDBY_60MIN", |avr, cx| async move {
        avr.zone(Zone::Main)?
            .set_auto_standby(AutoStandby::Minutes60)
            .await
    }),
    call!("AUTO_STANDBY_OFF", |avr, cx| async move {
        avr.zone(Zone::Main)?
            .set_auto_standby(AutoStandby::Off)
            .await
    }),
    call!("BACK_DOLBY_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::BackDolbyLeft)),
    call!("BACK_DOLBY_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::BackDolbyLeft)),
    call!("BACK_DOLBY_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::BackDolbyRight)),
    call!("BACK_DOLBY_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::BackDolbyRight)),
    call!("BASS_DOWN", |avr, cx| avr.tone().bass_down()),
    call!("BASS_SYNC_DOWN", |avr, cx| avr.sound().bass_sync_down()),
    call!("BASS_SYNC_UP", |avr, cx| avr.sound().bass_sync_up()),
    call!("BASS_UP", |avr, cx| avr.tone().bass_up()),
    call!("BT_LEVEL_DOWN", |avr, cx| avr
        .device()
        .bluetooth_level_down()),
    call!("BT_LEVEL_UP", |avr, cx| avr.device().bluetooth_level_up()),
    call!("BT_OUTPUT_MODE_BT_ONLY", |avr, cx| avr
        .device()
        .set_bluetooth_output(BluetoothOutputMode::BluetoothOnly)),
    call!("BT_OUTPUT_MODE_BT_SPEAKER", |avr, cx| avr
        .device()
        .set_bluetooth_output(
            BluetoothOutputMode::BluetoothAndSpeakers
        )),
    call!("BT_OUTPUT_MODE_TOGGLE", |avr, cx| avr
        .device()
        .set_bluetooth_output(
            if cx.state.device().bluetooth_output == Some(BluetoothOutputMode::BluetoothAndSpeakers)
            {
                BluetoothOutputMode::BluetoothOnly
            } else {
                BluetoothOutputMode::BluetoothAndSpeakers
            }
        )),
    call!("BT_TRANSMITTER_OFF", |avr, cx| avr
        .device()
        .set_bluetooth_transmitter(false)),
    call!("BT_TRANSMITTER_ON", |avr, cx| avr
        .device()
        .set_bluetooth_transmitter(true)),
    call!("BT_TRANSMITTER_TOGGLE", |avr, cx| avr
        .device()
        .set_bluetooth_transmitter(
            cx.state.device().bluetooth_transmitter != Some(true)
        )),
    call!("CENTER_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::Center)),
    call!("CENTER_HEIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::CenterHeight)),
    call!("CENTER_HEIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::CenterHeight)),
    call!("CENTER_SPREAD_OFF", |avr, cx| avr
        .sound()
        .set_center_spread(false)),
    call!("CENTER_SPREAD_ON", |avr, cx| avr
        .sound()
        .set_center_spread(true)),
    call!("CENTER_SPREAD_TOGGLE", |avr, cx| avr
        .sound()
        .set_center_spread(
            cx.state.device().sound_settings.center_spread != Some(true)
        )),
    call!("CENTER_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::Center)),
    call!("CENTER_WIDTH_DOWN", |avr, cx| avr
        .sound()
        .center_width_down()),
    call!("CENTER_WIDTH_UP", |avr, cx| avr.sound().center_width_up()),
    call!("CHANNEL_LEVEL_ADJUST_MENU", |avr, cx| avr
        .device()
        .channel_level_menu()),
    call!("CHANNEL_VOLUMES_RESET", |avr, cx| avr
        .sound()
        .reset_channel_volumes()),
    call!("CINEMA_EQ_OFF", |avr, cx| avr.sound().set_cinema_eq(false)),
    call!("CINEMA_EQ_ON", |avr, cx| avr.sound().set_cinema_eq(true)),
    call!("CINEMA_EQ_TOGGLE", |avr, cx| avr.sound().set_cinema_eq(
        cx.state.device().sound_settings.cinema_eq != Some(true)
    )),
    call!("CONTAINMENT_AMOUNT_DOWN", |avr, cx| avr
        .audyssey()
        .containment_amount_down()),
    call!("CONTAINMENT_AMOUNT_UP", |avr, cx| avr
        .audyssey()
        .containment_amount_up()),
    call!("DAC_FILTER_MODE_1", |avr, cx| avr
        .sound()
        .set_dac_filter(DacFilter::Mode1)),
    call!("DAC_FILTER_MODE_2", |avr, cx| avr
        .sound()
        .set_dac_filter(DacFilter::Mode2)),
    call!("DELAY_DOWN", |avr, cx| avr.sound().audio_delay_down()),
    call!("DELAY_TIME_DOWN", |avr, cx| avr.sound().delay_time_down()),
    call!("DELAY_TIME_UP", |avr, cx| avr.sound().delay_time_up()),
    call!("DELAY_UP", |avr, cx| avr.sound().audio_delay_up()),
    call!("DIALOG_CONTROL_DOWN", |avr, cx| avr
        .sound()
        .dialog_control_down()),
    call!("DIALOG_CONTROL_UP", |avr, cx| avr
        .sound()
        .dialog_control_up()),
    call!("DIALOG_ENHANCER_HIGH", |avr, cx| avr
        .sound()
        .set_dialog_enhancer(DialogEnhancerLevel::High)),
    call!("DIALOG_ENHANCER_LOW", |avr, cx| avr
        .sound()
        .set_dialog_enhancer(DialogEnhancerLevel::Low)),
    call!("DIALOG_ENHANCER_MEDIUM", |avr, cx| avr
        .sound()
        .set_dialog_enhancer(DialogEnhancerLevel::Medium)),
    call!("DIALOG_ENHANCER_OFF", |avr, cx| avr
        .sound()
        .set_dialog_enhancer(DialogEnhancerLevel::Off)),
    call!("DIGITAL_INPUT_AUTO", |avr, cx| avr
        .device()
        .set_digital_codec(DigitalCodec::Auto)),
    call!("DIGITAL_INPUT_DTS", |avr, cx| avr
        .device()
        .set_digital_codec(DigitalCodec::Dts)),
    call!("DIGITAL_INPUT_PCM", |avr, cx| avr
        .device()
        .set_digital_codec(DigitalCodec::Pcm)),
    call!("DIMENSION_DOWN", |avr, cx| avr.sound().dimension_down()),
    call!("DIMENSION_UP", |avr, cx| avr.sound().dimension_up()),
    call!("DIRAC_LIVE_FILTER_OFF", |avr, cx| async move {
        avr.dirac()?.set_filter(DiracFilter::Off).await
    }),
    call!("DIRAC_LIVE_FILTER_SLOT1", |avr, cx| async move {
        avr.dirac()?.set_filter(DiracFilter::Slot1).await
    }),
    call!("DIRAC_LIVE_FILTER_SLOT2", |avr, cx| async move {
        avr.dirac()?.set_filter(DiracFilter::Slot2).await
    }),
    call!("DIRAC_LIVE_FILTER_SLOT3", |avr, cx| async move {
        avr.dirac()?.set_filter(DiracFilter::Slot3).await
    }),
    call!("DISABLE_TONE_CONTROL", |avr, cx| avr
        .tone()
        .set_enabled(false)),
    call!("DRC_AUTO", |avr, cx| avr.sound().set_drc(Drc::Auto)),
    call!("DRC_HI", |avr, cx| avr.sound().set_drc(Drc::High)),
    call!("DRC_LOW", |avr, cx| avr.sound().set_drc(Drc::Low)),
    call!("DRC_MID", |avr, cx| avr.sound().set_drc(Drc::Mid)),
    call!("DRC_OFF", |avr, cx| avr.sound().set_drc(Drc::Off)),
    call!("DYNAMIC_EQ_OFF", |avr, cx| avr
        .audyssey()
        .set_dynamic_eq(false)),
    call!("DYNAMIC_EQ_ON", |avr, cx| avr
        .audyssey()
        .set_dynamic_eq(true)),
    call!("DYNAMIC_EQ_TOGGLE", |avr, cx| avr
        .audyssey()
        .set_dynamic_eq(cx.state.device().dynamic_eq != Some(true))),
    call!("DYNAMIC_VOLUME_HEAVY", |avr, cx| avr
        .audyssey()
        .set_dynamic_volume(DynamicVolume::Heavy)),
    call!("DYNAMIC_VOLUME_LIGHT", |avr, cx| avr
        .audyssey()
        .set_dynamic_volume(DynamicVolume::Light)),
    call!("DYNAMIC_VOLUME_MEDIUM", |avr, cx| avr
        .audyssey()
        .set_dynamic_volume(DynamicVolume::Medium)),
    call!("DYNAMIC_VOLUME_OFF", |avr, cx| avr
        .audyssey()
        .set_dynamic_volume(DynamicVolume::Off)),
    call!("EFFECT_SPEAKER_SELECTION_FLOOR", |avr, cx| avr
        .sound()
        .set_effect_speakers(EffectSpeakerSelection::Floor)),
    call!("EFFECT_SPEAKER_SELECTION_FRONT", |avr, cx| avr
        .sound()
        .set_effect_speakers(EffectSpeakerSelection::Front)),
    call!("EFFECT_SPEAKER_SELECTION_FRONT_HEIGHT", |avr, cx| avr
        .sound()
        .set_effect_speakers(EffectSpeakerSelection::FrontHeight)),
    call!("EFFECT_SPEAKER_SELECTION_FRONT_HEIGHT_WIDE", |avr, cx| avr
        .sound()
        .set_effect_speakers(
            EffectSpeakerSelection::FrontHeightAndWide
        )),
    call!("EFFECT_SPEAKER_SELECTION_FRONT_WIDE", |avr, cx| avr
        .sound()
        .set_effect_speakers(EffectSpeakerSelection::FrontWide)),
    call!("EFFECT_SPEAKER_SELECTION_HEIGHT_FLOOR", |avr, cx| avr
        .sound()
        .set_effect_speakers(EffectSpeakerSelection::HeightAndFloor)),
    call!("EFFECT_SPEAKER_SELECTION_SURROUND_BACK", |avr, cx| avr
        .sound()
        .set_effect_speakers(EffectSpeakerSelection::SurroundBack)),
];
