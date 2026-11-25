// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Tests of the Remote sessions in the controller.

use crate::controller::test_util::{
    DeviceAddr, LOOPBACK_CONFIG, LOOPBACK_KEY, Subscribed, connect, controller, entity_states,
    request, settle, use_closed_port,
};
use crate::controller::{Controller, R2EventMsg};
use crate::device::{DenonActor, DeviceStatus, Wants};
use actix::Addr;
use serde_json::json;
use std::time::Duration;
use uc_api::intg::ws::{R2Event, R2Request};

/// The entity states of the Remote `a`, once the receiver has sent them.
async fn first_states(controller: &Addr<Controller>) -> Vec<String> {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let states = entity_states(controller, "a").await;
        if !states.is_empty() {
            return states;
        }
        assert!(std::time::Instant::now() < end, "no entity states");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn actor(controller: &Addr<Controller>) -> Addr<DenonActor> {
    controller
        .send(DeviceAddr(LOOPBACK_KEY))
        .await
        .expect("send")
        .expect("actor")
}

async fn event(controller: &Addr<Controller>, event: R2Event) {
    controller
        .send(R2EventMsg {
            ws_id: "a".into(),
            event,
        })
        .await
        .expect("send");
}

#[actix::test]
async fn standby_ends_the_connection_wish_and_wake_starts_it_again() {
    use_closed_port();
    let controller = controller("session-standby", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    let ids = json!({ "entity_ids": [format!("media_player.{LOOPBACK_KEY}")] });
    request(&controller, "a", R2Request::SubscribeEvents, Some(ids))
        .await
        .expect("subscribe");
    settle(&controller, &[&remote]).await;
    let actor = actor(&controller).await;
    let state = actor.send(Wants).await.expect("send");
    assert!(state.want);
    assert_ne!(DeviceStatus::Connected, state.status);

    event(&controller, R2Event::EnterStandby).await;
    settle(&controller, &[&remote]).await;
    let state = actor.send(Wants).await.expect("send");
    assert!(!state.want);
    assert!(!state.reconnect);
    assert_eq!(DeviceStatus::Disconnected, state.status);

    remote.clear();
    event(&controller, R2Event::ExitStandby).await;
    settle(&controller, &[&remote]).await;
    assert_eq!(
        vec![json!({ "state": "CONNECTED" })],
        remote.filter("device_state")
    );
    assert!(actor.send(Wants).await.expect("send").want);
}

#[actix::test]
async fn a_subscribe_with_ids_limits_the_entity_states() {
    use_closed_port();
    let controller = controller("subscribe-ids", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    // Without a subscription, all entities.
    let all = first_states(&controller).await;
    assert!(all.len() > 2, "{all:?}");

    let ids = [
        format!("media_player.{LOOPBACK_KEY}"),
        format!("remote.{LOOPBACK_KEY}"),
    ];
    request(
        &controller,
        "a",
        R2Request::SubscribeEvents,
        Some(json!({ "entity_ids": ids })),
    )
    .await
    .expect("subscribe");
    settle(&controller, &[&remote]).await;
    assert_eq!(2, controller.send(Subscribed("a")).await.expect("send"));
    let mut states = entity_states(&controller, "a").await;
    states.sort_unstable();
    assert_eq!(ids.to_vec(), states);
}
