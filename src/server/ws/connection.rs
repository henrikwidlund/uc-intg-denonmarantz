// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! WebSocket connection handling for one Remote.

use crate::controller::{NewR2Session, R2SessionDisconnect, SendWsMessage};
use crate::errors::ServiceError;
use crate::server::ws::WsConn;
use actix::{Actor, ActorFutureExt, Addr, AsyncContext, Handler, WrapFuture};
use actix_ws::{AggregatedMessage, CloseCode, CloseReason, Session};
use log::{debug, error, info, warn};
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use uc_api::ws::{WsMessage, WsResultMsgData};

/// Longest wait for a free place in the session queue. The queue is full when the Remote does not
/// read.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages that can wait to be sent to one Remote.
const MAILBOX_CAPACITY: usize = 1024;
/// Invalid messages within [`INVALID_WINDOW`] before the connection is closed. A Remote does not
/// send invalid messages: many of them are a bug or another client.
const INVALID_LIMIT: u8 = 5;
const INVALID_WINDOW: Duration = Duration::from_secs(60);

/// Sends messages from the controller to the WebSocket connection.
///
/// Messages are sent one after another. Events have no id, so the Remote must get them in the
/// order they were created. A send only puts the frame in the session queue, so this is fast.
/// Responses to requests are sent here too.
struct WsSender {
    id: Arc<str>,
    session: Session,
    msg_tracing_out: bool,
    /// The session is closed: the messages that wait are dropped.
    closed: bool,
}

impl Actor for WsSender {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // A subscribe sends one message per entity: N per receiver. Changes that do not fit
        // are lost, because the controller sends only what changed.
        ctx.set_mailbox_capacity(MAILBOX_CAPACITY);
    }
}

impl Handler<SendWsMessage> for WsSender {
    type Result = ();

    fn handle(&mut self, msg: SendWsMessage, ctx: &mut Self::Context) {
        if self.closed {
            return;
        }
        let id = Arc::clone(&self.id);
        let msg_tracing_out = self.msg_tracing_out;
        let mut session = self.session.clone();

        // `wait` blocks the mailbox until the message is sent: keeps the order.
        ctx.wait(
            async move {
                let send = async {
                    match msg {
                        SendWsMessage::Msg(m) => {
                            WsConn::send(&id, msg_tracing_out, &m, &mut session).await
                        }
                        SendWsMessage::Text(text) => {
                            if msg_tracing_out {
                                debug!("[{id}] <- {text}");
                            }
                            session.text(text).await.map_err(|_| None)
                        }
                    }
                };
                let Ok(result) = timeout(SEND_TIMEOUT, send).await else {
                    // A lost message leaves an old state on the Remote. After a new connection,
                    // the Remote gets the full state again.
                    warn!("[{id}] Remote does not read, closing the connection");
                    close(session, None).await;
                    return true;
                };
                result.is_err()
            }
            .into_actor(self)
            .map(|closed, act, _| {
                if closed && !act.closed {
                    act.closed = true;
                    debug!(
                        "[{}] WebSocket closed, waiting messages are dropped",
                        act.id
                    );
                }
            }),
        );
    }
}

/// Counts invalid messages within [`INVALID_WINDOW`].
#[derive(Debug)]
pub(super) struct InvalidMessages {
    count: u8,
    since: Instant,
}

impl InvalidMessages {
    pub(super) fn new() -> Self {
        Self {
            count: 0,
            since: Instant::now(),
        }
    }

    /// Count one invalid message. True if the connection must be closed.
    fn add(&mut self, now: Instant) -> bool {
        if now.duration_since(self.since) > INVALID_WINDOW {
            self.since = now;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.count >= INVALID_LIMIT
    }
}

impl WsConn {
    /// Runs the message processing of the WebSocket connection until it is closed.
    ///
    /// - Registers the session with the controller.
    /// - Sends ping frames and closes the connection if the client does not answer.
    /// - Forwards requests, responses and events to the controller.
    pub(crate) async fn run(
        mut self,
        mut session: Session,
        mut stream: actix_ws::AggregatedMessageStream,
    ) {
        use actix_web::rt::time;
        use futures::StreamExt;

        // Only header-based authentication is used, so the authentication message is sent right away.
        // It has the driver version, so the Remote does not need to ask for it.
        let _ = session.text(self.authentication.clone()).await;

        let sender = WsSender {
            id: Arc::clone(&self.id),
            session: session.clone(),
            msg_tracing_out: self.msg_tracing_out,
            closed: false,
        }
        .start();

        if let Err(e) = self
            .controller_addr
            .send(NewR2Session {
                addr: sender.clone().recipient(),
                id: self.id.clone(),
            })
            .await
        {
            error!("Error registering new WebSocket connection: {e}");
            let _ = session
                .close(Some(CloseReason {
                    code: CloseCode::Error,
                    description: Some("internal error".into()),
                }))
                .await;
            return;
        }

        debug!("[{}] started", self.id);

        let mut hb_interval = time::interval(self.heartbeat.interval);
        loop {
            tokio::select! {
                _ = hb_interval.tick() => {
                    if Instant::now().duration_since(self.hb) > self.heartbeat.timeout {
                        info!("[{}] Closing connection due to failed heartbeat", self.id);
                        close(session, Some(CloseReason {
                            code: CloseCode::Away,
                            description: Some("heartbeat timeout".into()),
                        })).await;
                        break;
                    }
                    // Also ends the loop if the session was closed.
                    if !matches!(timeout(SEND_TIMEOUT, session.ping(b"")).await, Ok(Ok(()))) {
                        info!("[{}] Closing connection: Remote does not read", self.id);
                        close(session, None).await;
                        break;
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            if let Err(close_reason) = self.handle_stream_message(msg, &mut session, &sender).await {
                                if close_reason.is_some() {
                                    close(session, close_reason).await;
                                }
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            info!("[{}] Closing WebSocket: {e:?}", self.id);
                            close(session, None).await;
                            break;
                        }
                        None => {
                            debug!("[{}] Message stream ended, closing connection", self.id);
                            close(session, None).await;
                            break;
                        }
                    }
                }
            }
        }

        self.controller_addr.do_send(R2SessionDisconnect {
            id: self.id.clone(),
        });
        debug!("[{}] stopped", self.id);
    }

    /// Process a received WebSocket message.
    ///
    /// Returns `Err` if the connection must be closed.
    async fn handle_stream_message(
        &mut self,
        msg: AggregatedMessage,
        session: &mut Session,
        sender: &Addr<WsSender>,
    ) -> Result<(), Option<CloseReason>> {
        self.hb = Instant::now();
        match msg {
            AggregatedMessage::Text(text) => {
                if self.msg_tracing_in {
                    if is_backup_message(&text) {
                        debug!("[{}] -> <backup data hidden>", self.id);
                    } else {
                        debug!("[{}] -> {text}", self.id);
                    }
                }
                self.handle_text_message(&text, sender)
            }
            AggregatedMessage::Binary(_) => {
                debug!("[{}] Ignoring binary message", self.id);
                Ok(())
            }
            AggregatedMessage::Ping(bytes) => {
                if timeout(SEND_TIMEOUT, session.pong(&bytes)).await.is_err() {
                    info!("[{}] Closing connection: Remote does not read", self.id);
                    return Err(None);
                }
                Ok(())
            }
            AggregatedMessage::Pong(_) => Ok(()),
            AggregatedMessage::Close(reason) => {
                info!("[{}] Remote closed connection. Reason: {reason:?}", self.id);
                // The close is answered with a close, with the same code.
                Err(Some(reason.unwrap_or_else(|| CloseCode::Normal.into())))
            }
        }
    }

    /// An invalid message gets an error response only if it is a request with a readable id.
    /// The connection is closed after [`INVALID_LIMIT`] invalid messages within
    /// [`INVALID_WINDOW`].
    fn handle_text_message(
        &mut self,
        text: &str,
        sender: &Addr<WsSender>,
    ) -> Result<(), Option<CloseReason>> {
        let e = match serde_json::from_str::<WsMessage>(text) {
            Ok(msg) => {
                self.process_intg_api_message(msg, sender);
                return Ok(());
            }
            Err(e) => e,
        };
        if self.invalid.add(Instant::now()) {
            warn!(
                "[{}] {INVALID_LIMIT} invalid messages, closing the connection",
                self.id
            );
            return Err(Some(CloseReason {
                code: CloseCode::Policy,
                description: Some("Invalid JSON messages".into()),
            }));
        }
        // The error text does not contain values from `msg_data`.
        warn!("[{}] Invalid JSON message: {e}", self.id);
        if let Some(req_id) = invalid_request_id(text) {
            let error = ServiceError::BadRequest("Invalid JSON message".into());
            send_error_response(&self.id, req_id, error, sender);
        }
        Ok(())
    }

    /// Process a received Integration-API request, response or event message.
    ///
    /// A request runs in its own task, so a slow request does not stop reading the next
    /// messages. The responses are sent with `sender` and can come in a different order.
    fn process_intg_api_message(&self, mut msg: WsMessage, sender: &Addr<WsSender>) {
        match route(&msg) {
            Route::Request(req_id) => {
                let id = Arc::clone(&self.id);
                let sender = sender.clone();
                let request = Self::on_request(&id, req_id, &mut msg, &self.controller_addr);
                actix_web::rt::spawn(async move {
                    let result = match request {
                        Ok(request) => request.await.unwrap_or_else(|e| Err(e.into())),
                        Err(e) => Err(e),
                    };
                    match result {
                        Ok(Some(response)) => sender.do_send(SendWsMessage::Msg(response)),
                        Ok(None) => {}
                        Err(e) => {
                            warn!(
                                "[{id}] Error processing received message '{}': {e}",
                                msg.msg.as_deref().unwrap_or_default()
                            );
                            send_error_response(&id, req_id, e, &sender);
                        }
                    }
                });
            }
            Route::Response => {
                if let Err(e) = Self::on_response(&self.id, msg, &self.controller_addr) {
                    warn!("[{}] Error processing response: {e}", self.id);
                }
            }
            Route::Event => {
                if let Err(e) = Self::on_event(&self.id, msg, &self.controller_addr) {
                    warn!("[{}] Error processing event: {e}", self.id);
                }
            }
            Route::RequestWithoutId => warn!("[{}] Request without id, skipped", self.id),
            Route::UnknownKind { kind, req_id } => {
                warn!("[{}] Unsupported message kind: {kind}", self.id);
                if let Some(req_id) = req_id {
                    let e = ServiceError::BadRequest(format!("Unsupported message kind: {kind}"));
                    send_error_response(&self.id, req_id, e, sender);
                }
            }
            Route::MissingKind { req_id } => {
                warn!("[{}] Message without kind", self.id);
                if let Some(req_id) = req_id {
                    let e = ServiceError::BadRequest("Missing property: kind".into());
                    send_error_response(&self.id, req_id, e, sender);
                }
            }
        }
    }

    /// Send a message to the WebSocket session. Returns `Err` if the session is closed.
    async fn send(
        id: &str,
        msg_tracing_out: bool,
        msg: &WsMessage,
        session: &mut Session,
    ) -> Result<(), Option<CloseReason>> {
        match serde_json::to_string(msg) {
            Ok(text) => {
                if msg_tracing_out {
                    if is_backup_message(&text) {
                        debug!("[{id}] <- <backup data hidden>");
                    } else {
                        debug!("[{id}] <- {text}");
                    }
                }
                // The only error is a closed session. The caller logs it once.
                session.text(text).await.map_err(|_| None)
            }
            Err(e) => {
                error!("[{id}] Error serializing outgoing message: {e}");
                Ok(())
            }
        }
    }
}

/// How to handle a parsed message. Only a message with an id can get a response.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Request(u32),
    Response,
    Event,
    RequestWithoutId,
    /// Answered with an error if `req_id` is set.
    UnknownKind {
        kind: &'a str,
        req_id: Option<u32>,
    },
    /// Answered with an error if `req_id` is set.
    MissingKind {
        req_id: Option<u32>,
    },
}

fn route(msg: &WsMessage) -> Route<'_> {
    match (msg.kind.as_deref(), msg.id) {
        (Some("req"), Some(req_id)) => Route::Request(req_id),
        (Some("req"), None) => Route::RequestWithoutId,
        (Some("resp"), _) => Route::Response,
        (Some("event"), _) => Route::Event,
        (Some(kind), req_id) => Route::UnknownKind { kind, req_id },
        (None, req_id) => Route::MissingKind { req_id },
    }
}

/// The id of a message that `WsMessage` cannot parse, if it is a request with a readable id.
fn invalid_request_id(text: &str) -> Option<u32> {
    match serde_json::from_str(text) {
        Ok(RequestId {
            kind,
            id: Some(req_id),
        }) if kind.as_deref() == Some("req") => Some(req_id),
        _ => None,
    }
}

/// The fields of a request that an error response needs.
#[derive(serde::Deserialize)]
struct RequestId<'a> {
    /// Borrowed unless the text has escapes.
    #[serde(borrow)]
    kind: Option<Cow<'a, str>>,
    id: Option<u32>,
}

/// Returns `true` for a setup message with backup data. The backup text is not logged.
fn is_backup_message(text: &str) -> bool {
    text.contains("\"backup_data\"") || text.contains("\"restore_data\"")
}

/// Close the session. Does not wait long if the Remote does not read.
async fn close(session: Session, reason: Option<CloseReason>) {
    let _ = timeout(SEND_TIMEOUT, session.close(reason)).await;
}

fn send_error_response(id: &str, req_id: u32, error: ServiceError, sender: &Addr<WsSender>) {
    sender.do_send(SendWsMessage::Msg(service_error_to_ws_message(
        id, req_id, error,
    )));
}

fn service_error_to_ws_message(id: &str, req_id: u32, error: ServiceError) -> WsMessage {
    debug!("[{id}] Sending error response for: {error:?}");

    let (code, ws_err) = match error {
        ServiceError::InternalServerError(_) => {
            (500, WsResultMsgData::new("ERROR", "Internal server error"))
        }
        ServiceError::SerializationError(e) | ServiceError::BadRequest(e) => {
            (400, WsResultMsgData::new("BAD_REQUEST", e))
        }
        ServiceError::NotConnected => (
            503,
            WsResultMsgData::new("SERVICE_UNAVAILABLE", "Receiver is not connected"),
        ),
        ServiceError::ServiceUnavailable(e) => {
            (503, WsResultMsgData::new("SERVICE_UNAVAILABLE", e))
        }
        ServiceError::Timeout => (
            408,
            WsResultMsgData::new("TIMEOUT", "Receiver did not answer"),
        ),
        ServiceError::NotFound(e) => (404, WsResultMsgData::new("NOT_FOUND", e)),
        ServiceError::NotSupported(e) => (501, WsResultMsgData::new("NOT_IMPLEMENTED", e)),
        ServiceError::Blocked(e) => (409, WsResultMsgData::new("CONFLICT", e)),
    };

    WsMessage::error(req_id, code, ws_err)
}

#[cfg(test)]
mod tests {
    use super::{
        INVALID_LIMIT, INVALID_WINDOW, InvalidMessages, RequestId, Route, invalid_request_id,
        is_backup_message, route,
    };
    use std::time::{Duration, Instant};
    use uc_api::ws::WsMessage;

    #[test]
    fn only_a_message_with_an_id_gets_a_response() {
        let msg = |text: &str| serde_json::from_str::<WsMessage>(text).expect("json");
        let req = msg(r#"{"kind":"req","id":5,"msg":"get_version"}"#);
        assert_eq!(Route::Request(5), route(&req));
        let req = msg(r#"{"kind":"req","msg":"get_version"}"#);
        assert_eq!(Route::RequestWithoutId, route(&req));
        assert_eq!(
            Route::Response,
            route(&msg(r#"{"kind":"resp","req_id":1}"#))
        );
        assert_eq!(Route::Event, route(&msg(r#"{"kind":"event","msg":"x"}"#)));
        assert_eq!(
            Route::UnknownKind {
                kind: "foo",
                req_id: Some(3)
            },
            route(&msg(r#"{"kind":"foo","id":3}"#))
        );
        assert_eq!(
            Route::UnknownKind {
                kind: "foo",
                req_id: None
            },
            route(&msg(r#"{"kind":"foo"}"#))
        );
        assert_eq!(
            Route::MissingKind { req_id: Some(4) },
            route(&msg(r#"{"id":4,"msg":"x"}"#))
        );
        assert_eq!(
            Route::MissingKind { req_id: None },
            route(&msg(r#"{"msg":"x"}"#))
        );
    }

    #[test]
    fn an_invalid_message_gets_a_response_only_as_a_request_with_an_id() {
        // `cat` is not a valid event category, so `WsMessage` cannot read it.
        let text = r#"{"kind":"req","id":5,"msg":"get_version","cat":"x"}"#;
        assert!(serde_json::from_str::<WsMessage>(text).is_err());
        let id: RequestId<'_> = serde_json::from_str(text).expect("id");
        assert_eq!((Some("req"), Some(5)), (id.kind.as_deref(), id.id));
        // An escaped kind is read too.
        assert_eq!(
            Some(6),
            invalid_request_id(r#"{"kind":"r\u0065q","id":6,"cat":"x"}"#)
        );
        assert_eq!(Some(5), invalid_request_id(text));
        assert_eq!(
            None,
            invalid_request_id(r#"{"kind":"event","id":5,"cat":"x"}"#)
        );
        assert_eq!(None, invalid_request_id(r#"{"kind":"req","cat":"x"}"#));
        assert_eq!(None, invalid_request_id(r#"{"id":5,"cat":"x"}"#));
        assert_eq!(None, invalid_request_id("{not json"));
    }

    #[test]
    fn the_connection_closes_after_too_many_invalid_messages() {
        let mut invalid = InvalidMessages::new();
        let start = Instant::now();
        for _ in 1..INVALID_LIMIT {
            assert!(!invalid.add(start));
        }
        assert!(invalid.add(start));
    }

    #[test]
    fn invalid_messages_are_counted_again_after_the_window() {
        let mut invalid = InvalidMessages::new();
        let start = Instant::now();
        for _ in 1..INVALID_LIMIT {
            assert!(!invalid.add(start));
        }
        let later = start + INVALID_WINDOW + Duration::from_secs(1);
        assert!(!invalid.add(later));
    }

    #[test]
    fn backup_messages_are_found() {
        assert!(is_backup_message(
            r#"{"input_values":{"restore_data":"x"}}"#
        ));
        assert!(is_backup_message(r#"{"settings":[{"id":"backup_data"}]}"#));
        assert!(!is_backup_message(r#"{"input_values":{"pin":"1234"}}"#));
    }

    /// A WebSocket client over a plain TCP stream.
    mod raw {
        use std::io::{Read, Write};
        use std::net::{SocketAddr, TcpStream};
        use std::time::Duration;

        pub fn connect(addr: SocketAddr) -> TcpStream {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            s.write_all(
                b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
                  Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 13\r\n\r\n",
            )
            .expect("write");
            let mut head = Vec::new();
            let mut b = [0u8];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut b).expect("read");
                head.push(b[0]);
            }
            assert!(head.starts_with(b"HTTP/1.1 101"), "upgrade");
            s
        }

        /// Read one frame from the server. Returns the opcode and the payload.
        pub fn read(s: &mut TcpStream) -> (u8, Vec<u8>) {
            let mut h = [0u8; 2];
            s.read_exact(&mut h).expect("read");
            let len = match h[1] & 0x7f {
                126 => {
                    let mut l = [0u8; 2];
                    s.read_exact(&mut l).expect("read");
                    usize::from(u16::from_be_bytes(l))
                }
                127 => panic!("frame too long"),
                n => usize::from(n),
            };
            let mut payload = vec![0; len];
            s.read_exact(&mut payload).expect("read");
            (h[0] & 0x0f, payload)
        }

        /// Send a masked text frame, as the protocol requires for a client.
        pub fn send_text(s: &mut TcpStream, text: &str) {
            let mask = [1u8, 2, 3, 4];
            let len = u8::try_from(text.len()).expect("short");
            assert!(len < 126);
            let mut frame = vec![0x81, 0x80 | len];
            frame.extend_from_slice(&mask);
            frame.extend(text.bytes().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
            s.write_all(&frame).expect("write");
        }
    }

    /// Responses that the server sends until it closes, and the close code.
    type Answers = (Vec<serde_json::Value>, Option<u16>);

    #[actix::test]
    async fn errors_go_only_to_requests_with_an_id_and_invalid_messages_close() {
        use crate::configuration::HeartbeatSettings;
        use crate::controller::test_util::controller;
        use crate::server::{Authentication, ws_index};
        use actix_web::{App, HttpServer, web};

        let controller = web::Data::new(controller("ws-invalid", None));
        let heartbeat = web::Data::new(HeartbeatSettings::default());
        let metadata = serde_json::from_value(serde_json::json!({})).expect("metadata");
        let auth = web::Data::new(Authentication::new(&metadata));
        let server = HttpServer::new(move || {
            App::new()
                .app_data(heartbeat.clone())
                .app_data(auth.clone())
                .app_data(controller.clone())
                .service(ws_index)
        })
        .workers(1)
        .disable_signals()
        .bind(("127.0.0.1", 0))
        .expect("bind");
        let addr = server.addrs()[0];
        let server = server.run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        let (answers, close): Answers = tokio::task::spawn_blocking(move || {
            let mut s = raw::connect(addr);
            // The authentication message.
            assert_eq!(1, raw::read(&mut s).0);
            for text in [
                // No id: no response.
                r#"{"kind":"req","msg":"get_driver_version"}"#,
                r#"{"kind":"foo"}"#,
                r#"{"msg":"get_driver_version"}"#,
                // Invalid, no id: no response.
                "{not json",
                r#"{"kind":"event","cat":"x"}"#,
                r#"{"kind":"req","cat":"x"}"#,
                // Invalid with an id: an error response.
                r#"{"kind":"req","id":8,"msg":"x","cat":"x"}"#,
                // Valid with an id: a response.
                r#"{"kind":"req","id":7,"msg":"get_driver_version"}"#,
            ] {
                raw::send_text(&mut s, text);
            }
            // The responses. The driver also sends events and its own requests.
            let keep = |answers: &mut Vec<serde_json::Value>, payload: &[u8]| {
                let value: serde_json::Value = serde_json::from_slice(payload).expect("json");
                if value["kind"] == "resp" {
                    answers.push(value);
                }
            };
            let mut answers = Vec::new();
            while answers.len() < 2 {
                let (opcode, payload) = raw::read(&mut s);
                if opcode == 1 {
                    keep(&mut answers, &payload);
                }
            }
            // The 5th invalid message closes the connection.
            raw::send_text(&mut s, "[1,");
            loop {
                let (opcode, payload) = raw::read(&mut s);
                match opcode {
                    1 => keep(&mut answers, &payload),
                    8 => {
                        let code = payload
                            .first_chunk::<2>()
                            .map(|code| u16::from_be_bytes(*code));
                        return (answers, code);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("client");
        handle.stop(false).await;

        assert_eq!(Some(1008), close, "policy close code");
        let mut ids: Vec<_> = answers.iter().map(|a| a["req_id"].clone()).collect();
        ids.sort_by_key(|id| id.as_u64());
        assert_eq!(
            vec![serde_json::json!(7), serde_json::json!(8)],
            ids,
            "{answers:?}"
        );
        let error = answers.iter().find(|a| a["req_id"] == 8).expect("error");
        assert_eq!(400, error["code"]);
    }
}
