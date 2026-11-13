// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Typed calls of simple commands.

use super::Run;
use denon_marantz::proto::{
    Channel, SurroundLevelCompensation, Trigger, TunerBand, TunerMode, VideoProcessingMode, Zone,
};

pub const TABLE: &[(&str, Run)] = &[
    call!("SUBWOOFER_OFF", |avr, cx| avr.sound().set_subwoofer(false)),
    call!("SUBWOOFER_ON", |avr, cx| avr.sound().set_subwoofer(true)),
    call!("SUBWOOFER_TOGGLE", |avr, cx| avr.sound().set_subwoofer(
        !cx.state.device().sound_settings.subwoofer.unwrap_or(false)
    )),
    call!("SURROUND_BACK_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundBackLeft)),
    call!("SURROUND_BACK_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundBackLeft)),
    call!("SURROUND_BACK_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundBackRight)),
    call!("SURROUND_BACK_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundBackRight)),
    call!("SURROUND_DOLBY_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundDolbyLeft)),
    call!("SURROUND_DOLBY_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundDolbyLeft)),
    call!("SURROUND_DOLBY_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundDolbyRight)),
    call!("SURROUND_DOLBY_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundDolbyRight)),
    call!("SURROUND_HEIGHT_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundHeightLeft)),
    call!("SURROUND_HEIGHT_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundHeightLeft)),
    call!("SURROUND_HEIGHT_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundHeightRight)),
    call!("SURROUND_HEIGHT_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundHeightRight)),
    call!("SURROUND_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundLeft)),
    call!("SURROUND_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundLeft)),
    call!("SURROUND_LEVEL_COMP_HEAVY", |avr, cx| avr
        .sound()
        .set_surround_level(SurroundLevelCompensation::Heavy)),
    call!("SURROUND_LEVEL_COMP_LIGHT", |avr, cx| avr
        .sound()
        .set_surround_level(SurroundLevelCompensation::Light)),
    call!("SURROUND_LEVEL_COMP_MEDIUM", |avr, cx| avr
        .sound()
        .set_surround_level(SurroundLevelCompensation::Medium)),
    call!("SURROUND_LEVEL_COMP_OFF", |avr, cx| avr
        .sound()
        .set_surround_level(SurroundLevelCompensation::Off)),
    call!("SURROUND_MODE_AURO2DSURR", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("AURO2DSURR").await
    }),
    call!("SURROUND_MODE_AURO3D", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("AURO3D").await
    }),
    call!("SURROUND_MODE_AUTO", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("AUTO").await
    }),
    call!("SURROUND_MODE_DIRECT", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("DIRECT").await
    }),
    call!("SURROUND_MODE_DOLBY_DIGITAL", |avr, cx| async move {
        avr.zone(Zone::Main)?
            .select_sound_mode("DOLBY DIGITAL")
            .await
    }),
    call!("SURROUND_MODE_DOLBY_SURROUND", |avr, cx| async move {
        avr.zone(Zone::Main)?
            .select_sound_mode("DOLBY SURROUND")
            .await
    }),
    call!("SURROUND_MODE_DTS_SURROUND", |avr, cx| async move {
        avr.zone(Zone::Main)?
            .select_sound_mode("DTS SURROUND")
            .await
    }),
    call!("SURROUND_MODE_MCH_STEREO", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("MCH STEREO").await
    }),
    call!("SURROUND_MODE_MONO", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("MONO MOVIE").await
    }),
    call!("SURROUND_MODE_MULTI_CH_IN", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("MULTI CH IN").await
    }),
    call!("SURROUND_MODE_NEURAL_X", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("NEURAL:X").await
    }),
    call!("SURROUND_MODE_NEXT", |avr, cx| async move {
        avr.zone(Zone::Main)?.sound_mode_next().await
    }),
    call!("SURROUND_MODE_PREVIOUS", |avr, cx| async move {
        avr.zone(Zone::Main)?.sound_mode_previous().await
    }),
    call!("SURROUND_MODE_PURE_DIRECT", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("PURE DIRECT").await
    }),
    call!("SURROUND_MODE_STEREO", |avr, cx| async move {
        avr.zone(Zone::Main)?.select_sound_mode("STEREO").await
    }),
    call!("SURROUND_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::SurroundRight)),
    call!("SURROUND_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::SurroundRight)),
    call!("TACTILE_TRANSDUCER_LEVEL_DOWN", |avr, cx| avr
        .sound()
        .tactile_level_down()),
    call!("TACTILE_TRANSDUCER_LEVEL_UP", |avr, cx| avr
        .sound()
        .tactile_level_up()),
    call!("TACTILE_TRANSDUCER_LPF_100HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(100)),
    call!("TACTILE_TRANSDUCER_LPF_110HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(110)),
    call!("TACTILE_TRANSDUCER_LPF_120HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(120)),
    call!("TACTILE_TRANSDUCER_LPF_150HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(150)),
    call!("TACTILE_TRANSDUCER_LPF_180HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(180)),
    call!("TACTILE_TRANSDUCER_LPF_200HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(200)),
    call!("TACTILE_TRANSDUCER_LPF_250HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(250)),
    call!("TACTILE_TRANSDUCER_LPF_40HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(40)),
    call!("TACTILE_TRANSDUCER_LPF_60HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(60)),
    call!("TACTILE_TRANSDUCER_LPF_80HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(80)),
    call!("TACTILE_TRANSDUCER_LPF_90HZ", |avr, cx| avr
        .sound()
        .set_tactile_low_pass(90)),
    call!("TACTILE_TRANSDUCER_OFF", |avr, cx| avr
        .sound()
        .set_tactile_transducer(false)),
    call!("TACTILE_TRANSDUCER_ON", |avr, cx| avr
        .sound()
        .set_tactile_transducer(true)),
    call!("TACTILE_TRANSDUCER_TOGGLE", |avr, cx| avr
        .sound()
        .set_tactile_transducer(
            !cx.state
                .device()
                .sound_settings
                .tactile_transducer
                .unwrap_or(false)
        )),
    call!("TOP_FRONT_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::TopFrontLeft)),
    call!("TOP_FRONT_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::TopFrontLeft)),
    call!("TOP_FRONT_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::TopFrontRight)),
    call!("TOP_FRONT_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::TopFrontRight)),
    call!("TOP_MIDDLE_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::TopMiddleLeft)),
    call!("TOP_MIDDLE_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::TopMiddleLeft)),
    call!("TOP_MIDDLE_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::TopMiddleRight)),
    call!("TOP_MIDDLE_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::TopMiddleRight)),
    call!("TOP_REAR_LEFT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::TopRearLeft)),
    call!("TOP_REAR_LEFT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::TopRearLeft)),
    call!("TOP_REAR_RIGHT_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::TopRearRight)),
    call!("TOP_REAR_RIGHT_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::TopRearRight)),
    call!("TOP_SURROUND_DOWN", |avr, cx| avr
        .sound()
        .channel_volume_down(Channel::TopSurround)),
    call!("TOP_SURROUND_UP", |avr, cx| avr
        .sound()
        .channel_volume_up(Channel::TopSurround)),
    call!("TREBLE_DOWN", |avr, cx| avr.tone().treble_down()),
    call!("TREBLE_UP", |avr, cx| avr.tone().treble_up()),
    call!("TRIGGER1_OFF", |avr, cx| avr
        .device()
        .set_trigger(Trigger::One, false)),
    call!("TRIGGER1_ON", |avr, cx| avr
        .device()
        .set_trigger(Trigger::One, true)),
    call!("TRIGGER2_OFF", |avr, cx| avr
        .device()
        .set_trigger(Trigger::Two, false)),
    call!("TRIGGER2_ON", |avr, cx| avr
        .device()
        .set_trigger(Trigger::Two, true)),
    call!("TRIGGER3_OFF", |avr, cx| avr
        .device()
        .set_trigger(Trigger::Three, false)),
    call!("TRIGGER3_ON", |avr, cx| avr
        .device()
        .set_trigger(Trigger::Three, true)),
    call!("TUNER_BAND_AM", |avr, cx| avr
        .tuner()
        .set_band(TunerBand::Am)),
    call!("TUNER_BAND_FM", |avr, cx| avr
        .tuner()
        .set_band(TunerBand::Fm)),
    call!("TUNER_FREQUENCY_DOWN", |avr, cx| avr
        .tuner()
        .frequency_down()),
    call!("TUNER_FREQUENCY_UP", |avr, cx| avr.tuner().frequency_up()),
    call!("TUNER_MODE_AUTO", |avr, cx| avr
        .tuner()
        .set_mode(TunerMode::Auto)),
    call!("TUNER_MODE_MANUAL", |avr, cx| avr
        .tuner()
        .set_mode(TunerMode::Manual)),
    call!("TUNER_PRESET_DOWN", |avr, cx| avr.tuner().preset_down()),
    call!("TUNER_PRESET_UP", |avr, cx| avr.tuner().preset_up()),
    call!("VIDEO_PROCESSING_MODE_AUTO", |avr, cx| avr
        .video()
        .set_processing_mode(VideoProcessingMode::Auto)),
    call!("VIDEO_PROCESSING_MODE_BYPASS", |avr, cx| avr
        .video()
        .set_processing_mode(VideoProcessingMode::Bypass)),
    call!("VIDEO_PROCESSING_MODE_GAME", |avr, cx| avr
        .video()
        .set_processing_mode(VideoProcessingMode::Game)),
    call!("VIDEO_PROCESSING_MODE_MOVIE", |avr, cx| avr
        .video()
        .set_processing_mode(VideoProcessingMode::Movie)),
    call!("ZONE_FAVORITE_1", |avr, cx| avr.device().select_favorite(1)),
    call!("ZONE_FAVORITE_2", |avr, cx| avr.device().select_favorite(2)),
    call!("ZONE_FAVORITE_3", |avr, cx| avr.device().select_favorite(3)),
    call!("ZONE_FAVORITE_MEMORY_1", |avr, cx| avr
        .device()
        .store_favorite(1)),
    call!("ZONE_FAVORITE_MEMORY_2", |avr, cx| avr
        .device()
        .store_favorite(2)),
    call!("ZONE_FAVORITE_MEMORY_3", |avr, cx| avr
        .device()
        .store_favorite(3)),
];
