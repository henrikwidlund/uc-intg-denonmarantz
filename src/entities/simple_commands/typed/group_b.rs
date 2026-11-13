// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Typed calls of simple commands.

use super::Run;
use denon_marantz::proto::telnet::Command;
use denon_marantz::proto::{
    Channel, EffectSpeakerSelection, HdmiAudioDecode, Illumination, ImaxAudio, ImaxSubwooferOutput,
    Mdax, MultiEq, RemoteKey, Zone,
};

pub const TABLE: &[(&str, Run)] = &[
    call!(
        "EFFECT_SPEAKER_SELECTION_SURROUND_BACK_FRONT_HEIGHT",
        |avr, cx| avr
            .sound()
            .set_effect_speakers(EffectSpeakerSelection::SurroundBackAndFrontHeight)
    ),
    call!(
        "EFFECT_SPEAKER_SELECTION_SURROUND_BACK_FRONT_WIDE",
        |avr, cx| avr
            .sound()
            .set_effect_speakers(EffectSpeakerSelection::SurroundBackAndFrontWide)
    ),
    call!("ENABLE_TONE_CONTROL", |avr, cx| avr
        .tone()
        .set_enabled(true)),
    call!("FRONT_DOLBY_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontDolbyLeft)),
    call!("FRONT_DOLBY_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontDolbyLeft)),
    call!("FRONT_DOLBY_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontDolbyRight)),
    call!("FRONT_DOLBY_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontDolbyRight)),
    call!("FRONT_HEIGHT_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontHeightLeft)),
    call!("FRONT_HEIGHT_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontHeightLeft)),
    call!("FRONT_HEIGHT_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontHeightRight)),
    call!("FRONT_HEIGHT_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontHeightRight)),
    call!("FRONT_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontLeft)),
    call!("FRONT_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontLeft)),
    call!("FRONT_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontRight)),
    call!("FRONT_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontRight)),
    call!("FRONT_WIDE_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontWideLeft)),
    call!("FRONT_WIDE_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontWideLeft)),
    call!("FRONT_WIDE_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::FrontWideRight)),
    call!("FRONT_WIDE_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::FrontWideRight)),
    call!("GRAPHIC_EQ_OFF", |avr, cx| avr
        .sound()
        .set_graphic_eq(false)),
    call!("GRAPHIC_EQ_ON", |avr, cx| avr.sound().set_graphic_eq(true)),
    call!("GRAPHIC_EQ_TOGGLE", |avr, cx| avr.sound().set_graphic_eq(
        cx.state.device().sound_settings.graphic_eq != Some(true)
    )),
    call!("HDMI_AUDIO_DECODE_AMP", |avr, cx| avr
        .video()
        .set_hdmi_audio(HdmiAudioDecode::Amp)),
    call!("HDMI_AUDIO_DECODE_TV", |avr, cx| avr
        .video()
        .set_hdmi_audio(HdmiAudioDecode::Tv)),
    call!("HDMI_CEC_OFF", |avr, cx| avr
        .zone(Zone::Main)?
        .press(RemoteKey::HdmiCecOff)),
    call!("HDMI_CEC_ON", |avr, cx| avr
        .zone(Zone::Main)?
        .press(RemoteKey::HdmiCecOn)),
    call!("HEADPHONE_EQ_OFF", |avr, cx| avr
        .sound()
        .set_headphone_eq(false)),
    call!("HEADPHONE_EQ_ON", |avr, cx| avr
        .sound()
        .set_headphone_eq(true)),
    call!("HEADPHONE_EQ_TOGGLE", |avr, cx| avr
        .sound()
        .set_headphone_eq(
            cx.state.device().sound_settings.headphone_eq != Some(true)
        )),
    call!("ILLUMINATION_AUTO", |avr, cx| avr
        .device()
        .set_illumination(Illumination::Auto)),
    call!("ILLUMINATION_BRIGHT", |avr, cx| avr
        .device()
        .set_illumination(Illumination::Bright)),
    call!("ILLUMINATION_DARK", |avr, cx| avr
        .device()
        .set_illumination(Illumination::Dark)),
    call!("ILLUMINATION_DIM", |avr, cx| avr
        .device()
        .set_illumination(Illumination::Dim)),
    call!("ILLUMINATION_OFF", |avr, cx| avr
        .device()
        .set_illumination(Illumination::Off)),
    call!("IMAX_AUDIO_SETTINGS_AUTO", |avr, cx| avr
        .sound()
        .set_imax_audio(ImaxAudio::Auto)),
    call!("IMAX_AUDIO_SETTINGS_MANUAL", |avr, cx| avr
        .sound()
        .set_imax_audio(ImaxAudio::Manual)),
    call!("IMAX_AUDIO_SETTINGS_TOGGLE", |avr, cx| avr
        .sound()
        .set_imax_audio(
            if cx.state.device().sound_settings.imax_audio == Some(ImaxAudio::Auto) {
                ImaxAudio::Manual
            } else {
                ImaxAudio::Auto
            }
        )),
    call!("IMAX_HPF_100HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(100)),
    call!("IMAX_HPF_110HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(110)),
    call!("IMAX_HPF_120HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(120)),
    call!("IMAX_HPF_150HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(150)),
    call!("IMAX_HPF_180HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(180)),
    call!("IMAX_HPF_200HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(200)),
    call!("IMAX_HPF_250HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(250)),
    call!("IMAX_HPF_40HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(40)),
    call!("IMAX_HPF_60HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(60)),
    call!("IMAX_HPF_80HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(80)),
    call!("IMAX_HPF_90HZ", |avr, cx| avr
        .sound()
        .set_imax_high_pass(90)),
    call!("IMAX_LPF_100HZ", |avr, cx| avr
        .sound()
        .set_imax_low_pass(100)),
    call!("IMAX_LPF_110HZ", |avr, cx| avr
        .sound()
        .set_imax_low_pass(110)),
    call!("IMAX_LPF_120HZ", |avr, cx| avr
        .sound()
        .set_imax_low_pass(120)),
    call!("IMAX_LPF_150HZ", |avr, cx| avr
        .sound()
        .set_imax_low_pass(150)),
    call!("IMAX_LPF_180HZ", |avr, cx| avr
        .sound()
        .set_imax_low_pass(180)),
    call!("IMAX_LPF_200HZ", |avr, cx| avr
        .sound()
        .set_imax_low_pass(200)),
    call!("IMAX_LPF_250HZ", |avr, cx| avr
        .sound()
        .set_imax_low_pass(250)),
    call!("IMAX_LPF_80HZ", |avr, cx| avr.sound().set_imax_low_pass(80)),
    call!("IMAX_LPF_90HZ", |avr, cx| avr.sound().set_imax_low_pass(90)),
    call!("IMAX_SUBWOOFER_OFF", |avr, cx| avr
        .sound()
        .set_imax_subwoofer_mode(false)),
    call!("IMAX_SUBWOOFER_ON", |avr, cx| avr
        .sound()
        .set_imax_subwoofer_mode(true)),
    call!("IMAX_SUBWOOFER_OUTPUT_LFE", |avr, cx| avr
        .sound()
        .set_imax_subwoofer_output(ImaxSubwooferOutput::Lfe)),
    call!("IMAX_SUBWOOFER_OUTPUT_LFE_MAIN", |avr, cx| avr
        .sound()
        .set_imax_subwoofer_output(ImaxSubwooferOutput::LfeAndMain)),
    call!("INFO_MENU", |avr, cx| avr.send(Zone::Main, Command::Info)),
    call!("INPUT_8K", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("8K")),
    call!("INPUT_AUX1", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("AUX1")),
    call!("INPUT_AUX2", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("AUX2")),
    call!("INPUT_AUX3", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("AUX3")),
    call!("INPUT_AUX4", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("AUX4")),
    call!("INPUT_AUX5", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("AUX5")),
    call!("INPUT_AUX6", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("AUX6")),
    call!("INPUT_AUX7", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("AUX7")),
    call!("INPUT_BD", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("BD")),
    call!("INPUT_BT", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("BT")),
    call!("INPUT_CD", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("CD")),
    call!("INPUT_DVD", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("DVD")),
    call!("INPUT_GAME", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("GAME")),
    call!("INPUT_GAME1", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("GAME1")),
    call!("INPUT_GAME2", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("GAME2")),
    call!("INPUT_HD_RADIO", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("HDRADIO")),
    call!("INPUT_MPLAY", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("MPLAY")),
    call!("INPUT_NET", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("NET")),
    call!("INPUT_PHONO", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("PHONO")),
    call!("INPUT_TUNER", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("TUNER")),
    call!("INPUT_TV", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("TV")),
    call!("INPUT_SAT_CBL/CBL", |avr, cx| avr
        .zone(Zone::Main)?
        .select_source("SAT/CBL")),
    call!("LFE_DOWN", |avr, cx| avr.sound().lfe_level_down()),
    call!("LFE_UP", |avr, cx| avr.sound().lfe_level_up()),
    call!("LOUDNESS_MANAGEMENT_OFF", |avr, cx| avr
        .sound()
        .set_loudness_management(false)),
    call!("LOUDNESS_MANAGEMENT_ON", |avr, cx| avr
        .sound()
        .set_loudness_management(true)),
    call!("LOUDNESS_MANAGEMENT_TOGGLE", |avr, cx| avr
        .sound()
        .set_loudness_management(
            cx.state.device().sound_settings.loudness_management != Some(true)
        )),
    call!("MDAX_HIGH", |avr, cx| avr.sound().set_mdax(Mdax::High)),
    call!("MDAX_LOW", |avr, cx| avr.sound().set_mdax(Mdax::Low)),
    call!("MDAX_MEDIUM", |avr, cx| avr.sound().set_mdax(Mdax::Medium)),
    call!("MDAX_OFF", |avr, cx| avr.sound().set_mdax(Mdax::Off)),
    call!("MULTIEQ_BYPASS_LR", |avr, cx| avr
        .audyssey()
        .set_multi_eq(MultiEq::LrBypass)),
    call!("MULTIEQ_FLAT", |avr, cx| avr
        .audyssey()
        .set_multi_eq(MultiEq::Flat)),
];
