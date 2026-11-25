// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Tests of the Remote sessions in the controller.

use crate::controller::test_util::{
    DeviceAddr, DeviceKey, LOOPBACK_CONFIG, LOOPBACK_KEY, Subscribed, SubscribesAll, TestRemote,
    connect, controller, entity_states, request, settle, use_closed_port,
};
use crate::controller::{Controller, R2EventMsg, R2RequestMsg, R2SessionDisconnect};
use crate::device::{DenonActor, DeviceEvent, DeviceStatus, Wants};
use crate::entities::EntityKind;
use actix::Addr;
use serde_json::{Map, json};
use std::sync::Arc;
use std::time::Duration;
use uc_api::intg::ws::{R2Event, R2Request};
use uc_api::ws::WsMessage;

/// The entity states of the Remote `a`, once the receiver has sent them.
async fn first_states(controller: &Addr<Controller>, remote: &TestRemote) -> Vec<String> {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let states = entity_states(controller, remote).await;
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
    let all = first_states(&controller, &remote).await;
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
    let mut states = entity_states(&controller, &remote).await;
    states.sort_unstable();
    assert_eq!(ids.to_vec(), states);
}

async fn device_key(controller: &Addr<Controller>) -> Arc<str> {
    controller
        .send(DeviceKey(LOOPBACK_KEY))
        .await
        .expect("send")
        .expect("key")
}

/// A new media title of the media player.
fn title_change(key: Arc<str>, title: &str) -> DeviceEvent {
    let mut attrs = Map::new();
    attrs.insert("media_title".into(), title.into());
    DeviceEvent::Changes {
        key,
        changes: vec![(EntityKind::MediaPlayer, attrs)],
    }
}

fn request_msg(req_id: u32, request: R2Request) -> R2RequestMsg {
    R2RequestMsg {
        ws_id: "a".into(),
        req_id,
        request,
        msg_data: None,
    }
}

#[actix::test]
async fn an_entity_states_answer_comes_before_a_later_change() {
    use_closed_port();
    let controller = controller("states-order", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    let ids = json!({ "entity_ids": [format!("media_player.{LOOPBACK_KEY}")] });
    request(&controller, "a", R2Request::SubscribeEvents, Some(ids))
        .await
        .expect("subscribe");
    let key = device_key(&controller).await;

    // Both are in the mailbox of the controller before it handles the request.
    let answer = controller.send(request_msg(7, R2Request::GetEntityStates));
    controller.do_send(title_change(key, "probe"));
    let answer = answer.await.expect("send").expect("ok");
    assert!(answer.is_none(), "the controller sends the answer");
    settle(&controller, &[&remote]).await;

    let answer = remote
        .position(|m| m["kind"] == "resp" && m["req_id"] == 7)
        .expect("answer");
    let change = remote
        .position(|m| {
            m["msg"] == "entity_change" && m["msg_data"]["attributes"]["media_title"] == "probe"
        })
        .expect("change");
    assert!(answer < change, "answer {answer}, change {change}");
}

#[actix::test]
async fn a_device_state_answer_comes_before_a_later_device_state() {
    use_closed_port();
    let controller = controller("device-state-order", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    let key = device_key(&controller).await;
    settle(&controller, &[&remote]).await;
    remote.clear();

    let answer = controller.send(request_msg(8, R2Request::GetDeviceState));
    controller.do_send(DeviceEvent::Status {
        key,
        status: DeviceStatus::Error,
    });
    let answer = answer.await.expect("send").expect("ok");
    assert!(answer.is_none(), "the controller sends the answer");
    settle(&controller, &[&remote]).await;

    assert_eq!(
        vec![json!({ "state": "CONNECTED" }), json!({ "state": "ERROR" })],
        remote.filter("device_state")
    );
}

#[actix::test]
async fn a_subscribe_answer_comes_before_the_states_it_sends() {
    use_closed_port();
    let controller = controller("subscribe-order", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    first_states(&controller, &remote).await;
    // A change after the states were read is sent when the entity is subscribed.
    let id = format!("media_player.{LOOPBACK_KEY}");
    controller.do_send(title_change(device_key(&controller).await, "probe"));
    settle(&controller, &[&remote]).await;
    remote.clear();

    let answer = controller.send(R2RequestMsg {
        msg_data: Some(json!({ "entity_ids": [&id] })),
        ..request_msg(9, R2Request::SubscribeEvents)
    });
    let answer = answer.await.expect("send").expect("ok");
    assert!(answer.is_none(), "the controller sends the answer");
    settle(&controller, &[&remote]).await;

    let answer = remote
        .position(|m| m["kind"] == "resp" && m["req_id"] == 9 && m["code"] == 200)
        .expect("answer");
    let change = remote
        .position(|m| m["msg"] == "entity_change" && m["msg_data"]["entity_id"] == id.as_str())
        .expect("change");
    assert!(answer < change, "answer {answer}, change {change}");
}

#[test]
fn response_text_is_the_same_as_a_ws_message_response() {
    #[derive(serde::Serialize)]
    struct Data<'a> {
        name: &'a str,
        list: [u8; 2],
        none: Option<u8>,
    }
    let data = Data {
        name: "x",
        list: [1, 2],
        none: None,
    };
    let text = crate::controller::response_text(3, "result", &data).expect("text");
    let expected = serde_json::to_value(WsMessage::response(3, "result", &data)).expect("json");
    assert_eq!(
        expected,
        serde_json::from_str::<serde_json::Value>(&text).expect("json")
    );

    let text = crate::controller::response_text(4, "result", ()).expect("text");
    let expected = serde_json::to_value(WsMessage::response(4, "result", ())).expect("json");
    assert_eq!(
        expected,
        serde_json::from_str::<serde_json::Value>(&text).expect("json")
    );
}

#[actix::test]
async fn an_unsubscribe_answer_reaches_the_remote() {
    use_closed_port();
    let controller = controller("unsubscribe-answer", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    let answer = controller.send(R2RequestMsg {
        msg_data: Some(json!({ "entity_ids": [format!("media_player.{LOOPBACK_KEY}")] })),
        ..request_msg(10, R2Request::UnsubscribeEvents)
    });
    let answer = answer.await.expect("send").expect("ok");
    assert!(answer.is_none(), "the controller sends the answer");
    settle(&controller, &[&remote]).await;
    assert!(
        remote
            .position(|m| m["kind"] == "resp" && m["req_id"] == 10 && m["code"] == 200)
            .is_some()
    );
}

/// Wait until the actor plans a reconnect.
async fn until_reconnect(actor: &Addr<DenonActor>) {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while !actor.send(Wants).await.expect("send").reconnect {
        assert!(std::time::Instant::now() < end, "no reconnect");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[actix::test]
async fn standby_ends_the_retries_of_a_receiver_that_only_a_command_connected() {
    retries_of_an_unused_receiver_end("command-unused", false).await;
}

#[actix::test]
async fn the_end_of_a_session_ends_the_retries_of_a_receiver_that_only_a_command_connected() {
    retries_of_an_unused_receiver_end("command-unused-end", true).await;
}

/// A command connects a receiver that the Remote does not use. Standby, or the end of the
/// session, stops the connect tries.
async fn retries_of_an_unused_receiver_end(name: &str, end_session: bool) {
    use_closed_port();
    let controller = controller(name, Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    let command = json!({
        "entity_type": "media_player",
        "entity_id": format!("media_player.{LOOPBACK_KEY}"),
        "cmd_id": "on",
    });
    // The connect fails, so the command fails too.
    let _ = request(&controller, "a", R2Request::EntityCommand, Some(command)).await;
    let actor = actor(&controller).await;
    until_reconnect(&actor).await;
    let state = actor.send(Wants).await.expect("send");
    assert!(state.want);
    assert_eq!(DeviceStatus::Disconnected, state.status);

    if end_session {
        controller
            .send(R2SessionDisconnect { id: "a".into() })
            .await
            .expect("send");
    } else {
        event(&controller, R2Event::EnterStandby).await;
    }
    settle(&controller, &[&remote]).await;
    let state = actor.send(Wants).await.expect("send");
    assert!(!state.want);
    assert!(!state.reconnect);
}

#[actix::test]
async fn standby_does_not_change_a_receiver_that_was_not_asked_to_connect() {
    use_closed_port();
    let controller = controller("standby-unused", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    event(&controller, R2Event::EnterStandby).await;
    settle(&controller, &[&remote]).await;
    let state = actor(&controller).await.send(Wants).await.expect("send");
    assert!(!state.want);
    assert!(!state.keep_state);
}

#[actix::test]
async fn a_request_that_ends_standby_sends_the_missed_states_before_its_answer() {
    use_closed_port();
    let controller = controller("request-wake", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    let id = format!("media_player.{LOOPBACK_KEY}");
    request(
        &controller,
        "a",
        R2Request::SubscribeEvents,
        Some(json!({ "entity_ids": [&id] })),
    )
    .await
    .expect("subscribe");
    event(&controller, R2Event::EnterStandby).await;
    controller.do_send(title_change(device_key(&controller).await, "probe"));
    settle(&controller, &[&remote]).await;
    remote.clear();

    let answer = controller
        .send(request_msg(11, R2Request::GetDeviceState))
        .await
        .expect("send")
        .expect("ok");
    assert!(answer.is_none(), "the controller sends the answer");
    settle(&controller, &[&remote]).await;
    let change = remote
        .position(|m| {
            m["msg"] == "entity_change" && m["msg_data"]["attributes"]["media_title"] == "probe"
        })
        .expect("change");
    let answer = remote
        .position(|m| m["msg"] == "device_state")
        .expect("answer");
    assert!(change < answer, "change {change}, answer {answer}");
}

#[actix::test]
async fn subscribe_and_unsubscribe_without_msg_data_mean_all_entities() {
    use_closed_port();
    let controller = controller("subscribe-no-data", Some(LOOPBACK_CONFIG));
    let remote = connect(&controller, "a").await;
    request(&controller, "a", R2Request::SubscribeEvents, None)
        .await
        .expect("subscribe");
    assert!(controller.send(SubscribesAll("a")).await.expect("send"));
    assert!(
        actor(&controller)
            .await
            .send(Wants)
            .await
            .expect("send")
            .want
    );
    request(&controller, "a", R2Request::UnsubscribeEvents, None)
        .await
        .expect("unsubscribe");
    settle(&controller, &[&remote]).await;
    assert!(!controller.send(SubscribesAll("a")).await.expect("send"));
    assert!(
        !actor(&controller)
            .await
            .send(Wants)
            .await
            .expect("send")
            .want
    );
}
