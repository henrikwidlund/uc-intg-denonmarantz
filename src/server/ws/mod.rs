// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! WebSocket server for the Remote Two integration API.

use crate::configuration::{ENV_API_MSG_TRACING, HeartbeatSettings};
use crate::controller::{Controller, driver_version};
use actix::Addr;
use actix_web::{Error, HttpRequest, HttpResponse, get, web};
use bytestring::ByteString;
use log::debug;
use std::env;
use std::sync::Arc;
use std::time::Instant;
use uc_api::intg::IntegrationDriverUpdate;
use uc_api::ws::WsMessage;

mod connection;
mod events;
mod requests;
mod responses;

/// WebSocket connection of one Remote.
struct WsConn {
    /// Unique connection identifier, used for logging and to route messages in the [`Controller`].
    id: Arc<str>,
    /// Time of the last client activity.
    hb: Instant,
    /// [`Controller`] address for sending events and requests.
    controller_addr: Addr<Controller>,
    heartbeat: HeartbeatSettings,
    /// Log every incoming text message. Backup data is not logged.
    msg_tracing_in: bool,
    /// Log every outgoing message. Backup data is not logged.
    msg_tracing_out: bool,
    /// The `authentication` message, sent first.
    authentication: ByteString,
    /// Messages from the Remote that are not a JSON object.
    invalid: connection::InvalidMessages,
}

/// The `authentication` message with the driver version. It is the same for each connection.
#[derive(Clone)]
pub struct Authentication(pub ByteString);

impl Authentication {
    pub fn new(metadata: &IntegrationDriverUpdate) -> Self {
        let msg = WsMessage::response(0, "authentication", driver_version(metadata));
        let text = serde_json::to_string(&msg).unwrap_or_else(|_| {
            r#"{"kind":"resp","req_id":0,"code":200,"msg":"authentication"}"#.into()
        });
        Self(text.into())
    }
}

impl WsConn {
    fn new(
        client_id: String,
        controller_addr: Addr<Controller>,
        heartbeat: HeartbeatSettings,
        authentication: ByteString,
    ) -> Self {
        let msg_tracing = env::var(ENV_API_MSG_TRACING).unwrap_or_default();
        Self {
            authentication,
            invalid: connection::InvalidMessages::new(),
            id: client_id.into(),
            hb: Instant::now(),
            controller_addr,
            heartbeat,
            msg_tracing_in: msg_tracing == "all" || msg_tracing == "in",
            msg_tracing_out: msg_tracing == "all" || msg_tracing == "out",
        }
    }
}

/// HTTP endpoint for the WebSocket upgrade.
#[get("/")]
pub async fn ws_index(
    request: HttpRequest,
    stream: web::Payload,
    heartbeat: web::Data<HeartbeatSettings>,
    controller: web::Data<Addr<Controller>>,
    authentication: web::Data<Authentication>,
) -> Result<HttpResponse, Error> {
    // Use peer IP:port as unique client identifier.
    let client_id = request
        .peer_addr()
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), |addr| addr.to_string());
    debug!("New WebSocket connection from: {client_id}");

    let (resp, session, msg_stream) = actix_ws::handle(&request, stream)?;

    // Remote messages are small. The default frame size of 64 KiB is enough.
    let stream = msg_stream.aggregate_continuations();

    let conn = WsConn::new(
        client_id,
        controller.get_ref().clone(),
        *heartbeat.get_ref(),
        authentication.0.clone(),
    );

    actix_web::rt::spawn(async move {
        conn.run(session, stream).await;
    });

    Ok(resp)
}
