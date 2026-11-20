// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Handler for [`R2RequestMsg`].

use crate::controller::handler::setup::{SetDriverUserDataMsg, SetupDriverMsg};
use crate::controller::{Controller, R2RequestMsg, operation_mode};
use crate::errors::ServiceError;
use crate::util::{DeserializeMsgData, return_fut_err, return_fut_ok};
use crate::{APP_VERSION, built_info};
use actix::{Handler, ResponseFuture};
use log::debug;
use serde::Deserialize;
use serde_json::Value;
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

/// `msg_data` of `subscribe_events` and `unsubscribe_events`. No ids means all entities.
#[derive(Deserialize)]
struct EntityIds {
    #[serde(default)]
    entity_ids: Vec<String>,
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
        // `exit_standby`: the receivers it uses connect now.
        let Some(session) = self.sessions.get_mut(&msg.ws_id) else {
            return_fut_err!(ServiceError::NotFound("No session found".into()));
        };
        let woke = !session.awake();
        session.standby = false;
        session.paused = false;
        if woke {
            self.update_connections(false);
        }

        let req_id = msg.req_id;
        // Answered with an event, so it has no response message.
        if msg.request == R2Request::GetDeviceState {
            return_fut_ok!(Some(super::super::device_state_event(self.device_state)));
        }
        let Some(resp_msg) = msg.request.get_message() else {
            return_fut_err!(ServiceError::InternalServerError(format!(
                "No response message for {}",
                msg.request
            )));
        };

        // Requests that are allowed in setup flow and running mode.
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
                    .and_then(|data| self.handle(SetupDriverMsg { ws_id, data }, ctx));
                return Box::pin(async move {
                    result?;
                    Ok(Some(WsMessage::response(req_id, resp_msg, Value::Null)))
                });
            }
            R2Request::SetDriverUserData => {
                let ws_id = msg.ws_id.to_string();
                let result = msg
                    .deserialize()
                    .map_err(ServiceError::from)
                    .and_then(|data| self.handle(SetDriverUserDataMsg { ws_id, data }, ctx));
                return Box::pin(async move {
                    result?;
                    Ok(Some(WsMessage::response(req_id, resp_msg, Value::Null)))
                });
            }
            _ => {}
        }

        // The other requests need a configuration: the running mode or a setup flow.
        if self
            .sm_consume(&msg.ws_id, &operation_mode::Input::R2Request, ctx)
            .is_err()
        {
            return_fut_err!(ServiceError::ServiceUnavailable(
                "Request cannot be handled: setup required".into()
            ));
        }

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
                let states = self.entity_states(&msg.ws_id);
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, states)));
            }
            R2Request::SubscribeEvents => {
                let ws_id = msg.ws_id.clone();
                match msg.deserialize::<EntityIds>() {
                    Ok(subscribe) => self.subscribe(&ws_id, subscribe.entity_ids),
                    Err(e) => return_fut_err!(e.into()),
                }
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, Value::Null)));
            }
            R2Request::UnsubscribeEvents => {
                let ws_id = msg.ws_id.clone();
                match msg.deserialize::<EntityIds>() {
                    Ok(subscribe) => self.unsubscribe(&ws_id, &subscribe.entity_ids),
                    Err(e) => return_fut_err!(e.into()),
                }
                return_fut_ok!(Some(WsMessage::response(req_id, resp_msg, Value::Null)));
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
