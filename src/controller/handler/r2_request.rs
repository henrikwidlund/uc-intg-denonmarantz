// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Handler for [`R2RequestMsg`].

use crate::controller::devices::ConnectTrigger;
use crate::controller::handler::setup::{SetDriverUserDataMsg, SetupDriverMsg};
use crate::controller::{Controller, R2RequestMsg, SendWsMessage};
use crate::errors::ServiceError;
use crate::util::{DeserializeMsgData, return_fut_err, return_fut_ok};
use crate::{APP_VERSION, built_info};
use actix::{Handler, ResponseFuture};
use log::debug;
use serde::Deserialize;
use strum::EnumMessage;
use uc_api::intg::ws::AvailableEntitiesFilter;
use uc_api::intg::ws::{AvailableEntitiesMsgData, DriverVersionMsgData, R2Request};
use uc_api::intg::{IntegrationDriverUpdate, IntegrationVersion};
use uc_api::ws::WsMessage;

/// Integration-API version from the `uc_api` dependency.
fn api_version() -> &'static str {
    built_info::DEPENDENCIES
        .iter()
        .find(|p| p.0 == "uc_api")
        .map_or("?", |v| v.1)
}

/// `msg_data` of `get_available_entities`.
#[derive(Deserialize)]
struct AvailableEntitiesRequest {
    filter: Option<AvailableEntitiesFilter>,
}

/// `msg_data` of `subscribe_events` and `unsubscribe_events`. Without `msg_data` or without
/// `entity_ids`: all entities. An empty list: all entities for subscribe, none for unsubscribe.
#[derive(Default, Deserialize)]
struct EntityIds {
    entity_ids: Option<Vec<String>>,
}

impl EntityIds {
    /// Read the optional `msg_data`. `null` is the same as no `msg_data`.
    fn from_msg_data(msg_data: Option<serde_json::Value>) -> Result<Self, serde_json::Error> {
        match msg_data {
            None | Some(serde_json::Value::Null) => Ok(Self::default()),
            Some(data) => serde_json::from_value(data),
        }
    }
}

/// The name and the versions of the driver.
pub(crate) fn driver_version(metadata: &IntegrationDriverUpdate) -> DriverVersionMsgData {
    DriverVersionMsgData {
        name: metadata.name.as_ref().and_then(|n| n.get("en").cloned()),
        version: Some(IntegrationVersion {
            api: Some(api_version().into()),
            driver: Some(APP_VERSION.into()),
        }),
    }
}

impl Handler<R2RequestMsg> for Controller {
    type Result = ResponseFuture<Result<Option<WsMessage>, ServiceError>>;

    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, msg: R2RequestMsg, ctx: &mut Self::Context) -> Self::Result {
        debug!("R2RequestMsg: {:?}", msg.request);
        // A Remote that sends a request is not in standby. The core can send a command before
        // `exit_standby`: the receivers it uses connect now, and the Remote gets the states that
        // changed during standby.
        let Some(session) = self.sessions.get_mut(&msg.ws_id) else {
            return_fut_err!(ServiceError::NotFound("No session found".into()));
        };
        let woke = !session.awake();
        let was_standby = session.standby;
        session.standby = false;
        session.paused = false;
        if woke {
            self.update_connections(ConnectTrigger::Wake);
        }
        if was_standby {
            self.send_missed_states(&msg.ws_id);
        }

        let req_id = msg.req_id;
        // Answered with an event, so it has no response message. Sent from here, so that a later
        // `device_state` event cannot come first.
        if msg.request == R2Request::GetDeviceState {
            let event = super::super::device_state_event(self.device_state);
            self.send_r2_response(event, &msg.ws_id);
            return_fut_ok!(None);
        }
        let Some(resp_msg) = msg.request.get_message() else {
            return_fut_err!(ServiceError::InternalServerError(format!(
                "No response message for {}",
                msg.request
            )));
        };

        // Requests that do not need the receivers.
        match msg.request {
            R2Request::GetDriverVersion => {
                return_fut_ok!(Some(WsMessage::response(
                    req_id,
                    resp_msg,
                    driver_version(&self.drv_metadata),
                )));
            }
            R2Request::GetDriverMetadata => {
                return_fut_ok!(Some(WsMessage::response(
                    req_id,
                    resp_msg,
                    &self.drv_metadata
                )));
            }
            // The setup messages are handled here and not sent to the mailbox again, so a later
            // event (for example abort_driver_setup) cannot be handled first.
            R2Request::SetupDriver => {
                let ws_id = msg.ws_id.to_string();
                let result = msg
                    .deserialize()
                    .map_err(ServiceError::from)
                    .and_then(|data| {
                        let ack = Some((req_id, resp_msg));
                        self.handle(SetupDriverMsg { ws_id, data, ack }, ctx)
                    });
                // The handler sends the acknowledgment.
                return Box::pin(std::future::ready(result.map(|()| None)));
            }
            R2Request::SetDriverUserData => {
                let ws_id = msg.ws_id.to_string();
                let result = msg
                    .deserialize()
                    .map_err(ServiceError::from)
                    .and_then(|data| {
                        let ack = Some((req_id, resp_msg));
                        self.handle(SetDriverUserDataMsg { ws_id, data, ack }, ctx)
                    });
                // The handler sends the acknowledgment.
                return Box::pin(std::future::ready(result.map(|()| None)));
            }
            _ => {}
        }

        // Without a configuration, the other requests see no receivers: empty lists and states, and
        // "not found" for a command.
        match msg.request {
            R2Request::GetAvailableEntities => {
                let filter = match msg.msg_data {
                    Some(data) => match serde_json::from_value::<AvailableEntitiesRequest>(data) {
                        Ok(request) => request.filter,
                        Err(e) => return_fut_err!(e.into()),
                    },
                    None => None,
                };
                let entity_type = filter.as_ref().and_then(|f| f.entity_type);
                let msg_data = AvailableEntitiesMsgData {
                    available_entities: self.available_entities(entity_type),
                    filter,
                };
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, msg_data)));
            }
            R2Request::GetEntityStates => {
                // Sent from here, so that a later `entity_change` event cannot come first.
                let text = {
                    let states = self.entity_states(&msg.ws_id);
                    super::super::response_text(req_id, resp_msg, &states)
                };
                match text {
                    Ok(text) => self.send_r2_response(SendWsMessage::Text(text), &msg.ws_id),
                    Err(e) => return_fut_err!(e),
                }
                return_fut_ok!(None);
            }
            R2Request::SubscribeEvents => {
                let ws_id = msg.ws_id.clone();
                let subscribe = match EntityIds::from_msg_data(msg.msg_data) {
                    Ok(subscribe) => subscribe,
                    Err(e) => return_fut_err!(e.into()),
                };
                // The answer comes before the states that the subscription sends.
                if let Err(e) = self.respond(&ws_id, req_id, resp_msg, ()) {
                    return_fut_err!(e);
                }
                self.subscribe(&ws_id, subscribe.entity_ids.filter(|ids| !ids.is_empty()));
                return_fut_ok!(None);
            }
            R2Request::UnsubscribeEvents => {
                let ws_id = msg.ws_id.clone();
                let unsubscribe = match EntityIds::from_msg_data(msg.msg_data) {
                    Ok(unsubscribe) => unsubscribe,
                    Err(e) => return_fut_err!(e.into()),
                };
                if let Err(e) = self.respond(&ws_id, req_id, resp_msg, ()) {
                    return_fut_err!(e);
                }
                self.unsubscribe(&ws_id, unsubscribe.entity_ids.as_deref());
                return_fut_ok!(None);
            }
            R2Request::EntityCommand => self.entity_command(msg, resp_msg),
            R2Request::BrowseMedia | R2Request::SearchMedia => {
                return_fut_err!(ServiceError::NotSupported(msg.request.to_string()))
            }
            // Handled above. Listed so that the compiler reports new variants.
            R2Request::GetDriverVersion
            | R2Request::GetDriverMetadata
            | R2Request::GetDeviceState
            | R2Request::SetupDriver
            | R2Request::SetDriverUserData => {
                return_fut_err!(ServiceError::InternalServerError(format!(
                    "Request {} not handled",
                    msg.request
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EntityIds;
    use serde_json::json;

    #[test]
    fn entity_ids_without_a_list_are_none() {
        for data in [
            None,
            Some(json!(null)),
            Some(json!({})),
            Some(json!({ "entity_ids": null })),
        ] {
            let ids = EntityIds::from_msg_data(data.clone()).expect("ids");
            assert_eq!(None, ids.entity_ids, "{data:?}");
        }
    }

    #[test]
    fn entity_ids_keep_the_list() {
        let ids = EntityIds::from_msg_data(Some(json!({ "entity_ids": [] }))).expect("ids");
        assert_eq!(Some(Vec::new()), ids.entity_ids);
        let ids = EntityIds::from_msg_data(Some(json!({ "entity_ids": ["x"] }))).expect("ids");
        assert_eq!(Some(vec!["x".to_owned()]), ids.entity_ids);
    }

    #[test]
    fn entity_ids_that_are_not_a_list_are_an_error() {
        assert!(EntityIds::from_msg_data(Some(json!({ "entity_ids": "x" }))).is_err());
    }
}
