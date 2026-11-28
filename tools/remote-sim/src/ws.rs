// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! WebSocket client that acts as the Remote.

use crate::Fail;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Bytes, Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Longest time to wait for the answer to a request. The driver must answer in less than 10 s.
const ANSWER_LIMIT: Duration = Duration::from_secs(15);

pub struct Client {
    sink: SplitSink<Ws, Message>,
    stream: SplitStream<Ws>,
    /// Local address of the socket. The driver logs it as the id of the session.
    local: SocketAddr,
    next_id: u32,
    /// Latest attributes of each entity, from `entity_change` events and entity states.
    attrs: HashMap<String, Map<String, Value>>,
    /// Setup events that were not read yet.
    setup: VecDeque<Value>,
    /// Number of `entity_change` events with `media_position`, per entity.
    position_changes: HashMap<String, usize>,
}

impl Client {
    pub async fn connect(port: u16) -> Result<Self, Fail> {
        let (ws, _) = connect_async(format!("ws://127.0.0.1:{port}/")).await?;
        let local = match ws.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.local_addr()?,
            _ => return Err(Fail::new("not a plain TCP connection")),
        };
        let (sink, stream) = ws.split();
        let mut client = Self {
            sink,
            stream,
            local,
            next_id: 0,
            attrs: HashMap::new(),
            setup: VecDeque::new(),
            position_changes: HashMap::new(),
        };
        // The driver sends the authentication answer at once.
        client
            .pump(Instant::now() + Duration::from_millis(300))
            .await?;
        Ok(client)
    }

    /// Local address of the socket, as the driver logs it.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The next frame. `None` at `deadline`. Fails if the connection ends.
    async fn next_frame(&mut self, deadline: Instant) -> Result<Option<Message>, Fail> {
        let wait = deadline.saturating_duration_since(Instant::now());
        match timeout(wait, self.stream.next()).await {
            Err(_) => Ok(None),
            Ok(None) => Err(Fail::new("connection closed")),
            Ok(Some(Err(e))) => Err(e.into()),
            Ok(Some(Ok(frame))) => Ok(Some(frame)),
        }
    }

    /// Parse a text message and record its event.
    fn text_message(&mut self, text: &str) -> Result<Value, Fail> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| Fail::new(format!("invalid JSON from the driver: {e}")))?;
        self.record(&value);
        Ok(value)
    }

    /// Read messages until `deadline`. Returns the first text message.
    async fn pump(&mut self, deadline: Instant) -> Result<Option<Value>, Fail> {
        loop {
            match self.next_frame(deadline).await? {
                None => return Ok(None),
                Some(Message::Close(_)) => return Err(Fail::new("connection closed")),
                Some(Message::Text(text)) => return self.text_message(text.as_str()).map(Some),
                // Pings are answered by the library.
                Some(_) => {}
            }
        }
    }

    /// Send a ping with `data` and wait up to `wait` for the pong with the same data. Returns the
    /// time until the pong.
    pub async fn ping(&mut self, data: &[u8], wait: Duration) -> Result<Duration, Fail> {
        let start = Instant::now();
        self.sink
            .send(Message::Ping(Bytes::copy_from_slice(data)))
            .await?;
        let deadline = start + wait;
        loop {
            match self.next_frame(deadline).await? {
                None => return Err(Fail::new(format!("no pong in {} ms", wait.as_millis()))),
                Some(frame) if is_pong(&frame, data) => return Ok(start.elapsed()),
                Some(Message::Close(_)) => return Err(Fail::new("connection closed")),
                Some(Message::Text(text)) => {
                    self.text_message(text.as_str())?;
                }
                Some(_) => {}
            }
        }
    }

    /// Send a close frame with `code` and `reason` and wait up to `wait` for the close frame of
    /// the driver. Returns that frame. `None`: a close frame without a code.
    pub async fn close(
        mut self,
        code: u16,
        reason: &'static str,
        wait: Duration,
    ) -> Result<Option<CloseFrame>, Fail> {
        let frame = CloseFrame {
            code: code.into(),
            reason: Utf8Bytes::from_static(reason),
        };
        self.sink.send(Message::Close(Some(frame))).await?;
        let deadline = Instant::now() + wait;
        loop {
            match self.next_frame(deadline).await? {
                None => {
                    return Err(Fail::new(format!(
                        "no close frame in {} ms",
                        wait.as_millis()
                    )));
                }
                Some(Message::Close(frame)) => return Ok(frame),
                Some(Message::Text(text)) => {
                    self.text_message(text.as_str())?;
                }
                Some(_) => {}
            }
        }
    }

    fn record(&mut self, message: &Value) {
        if message["kind"] != "event" {
            return;
        }
        let data = &message["msg_data"];
        match message["msg"].as_str() {
            Some("entity_change") => {
                if let (Some(id), Some(attrs)) =
                    (data["entity_id"].as_str(), data["attributes"].as_object())
                {
                    self.attrs
                        .entry(id.to_owned())
                        .or_default()
                        .extend(attrs.clone());
                    if attrs.contains_key("media_position") {
                        *self.position_changes.entry(id.to_owned()).or_default() += 1;
                    }
                }
            }
            Some("driver_setup_change") => self.setup.push_back(data.clone()),
            _ => {}
        }
    }

    pub async fn send(&mut self, value: Value) -> Result<(), Fail> {
        self.sink.send(Message::text(value.to_string())).await?;
        Ok(())
    }

    /// Send the frames that wait in the write buffer, for example after a dropped send.
    pub async fn flush(&mut self) -> Result<(), Fail> {
        self.sink.flush().await?;
        Ok(())
    }

    /// Send a request and wait for the response.
    pub async fn request(&mut self, msg: &str, data: Value) -> Result<Value, Fail> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "kind": "req", "id": id, "msg": msg, "msg_data": data }))
            .await?;
        let deadline = Instant::now() + ANSWER_LIMIT;
        loop {
            match self.pump(deadline).await? {
                None => return Err(Fail::new(format!("no answer to {msg} in 15 s"))),
                Some(v) if v["kind"] == "resp" && v["req_id"] == id => return Ok(v),
                Some(_) => {}
            }
        }
    }

    /// A request that must answer with code 200. Returns `msg_data`.
    pub async fn request_ok(&mut self, msg: &str, data: Value) -> Result<Value, Fail> {
        let response = self.request(msg, data).await?;
        if response["code"] == 200 {
            Ok(response["msg_data"].clone())
        } else {
            Err(Fail::new(format!(
                "{msg}: code {}, {}",
                response["code"], response["msg_data"]
            )))
        }
    }

    pub async fn send_event(&mut self, msg: &str) -> Result<(), Fail> {
        self.send(json!({ "kind": "event", "msg": msg, "cat": "DEVICE" }))
            .await
    }

    /// Send an entity command. Returns the response and the time that the driver took.
    pub async fn command(
        &mut self,
        entity_type: &str,
        entity_id: &str,
        cmd_id: &str,
        params: Value,
    ) -> Result<(Value, Duration), Fail> {
        let start = Instant::now();
        let response = self
            .request(
                "entity_command",
                json!({
                    "entity_type": entity_type,
                    "entity_id": entity_id,
                    "cmd_id": cmd_id,
                    "params": params,
                }),
            )
            .await?;
        Ok((response, start.elapsed()))
    }

    /// A command that must work and be answered in less than 10 s.
    pub async fn command_ok(
        &mut self,
        entity_type: &str,
        entity_id: &str,
        cmd_id: &str,
        params: Value,
    ) -> Result<(), Fail> {
        let (response, took) = self.command(entity_type, entity_id, cmd_id, params).await?;
        if response["code"] != 200 {
            return Err(Fail::new(format!(
                "{cmd_id} on {entity_id}: code {}, {}",
                response["code"], response["msg_data"]
            )));
        }
        if took > Duration::from_secs(10) {
            return Err(Fail::new(format!("{cmd_id} took {} ms", took.as_millis())));
        }
        Ok(())
    }

    /// Read the states of all entities into the cache.
    pub async fn refresh_states(&mut self) -> Result<(), Fail> {
        let states = self.request_ok("get_entity_states", Value::Null).await?;
        for state in states.as_array().into_iter().flatten() {
            if let (Some(id), Some(attrs)) =
                (state["entity_id"].as_str(), state["attributes"].as_object())
            {
                self.attrs
                    .entry(id.to_owned())
                    .or_default()
                    .extend(attrs.clone());
            }
        }
        Ok(())
    }

    /// Forget the cached states, for example after a mode change.
    pub fn forget_states(&mut self) {
        self.attrs.clear();
    }

    /// Number of `entity_change` events with `media_position` of an entity so far.
    pub fn position_count(&self, entity: &str) -> usize {
        self.position_changes.get(entity).copied().unwrap_or(0)
    }

    pub fn attr(&self, entity: &str, attr: &str) -> Option<&Value> {
        self.attrs.get(entity)?.get(attr)
    }

    pub fn text(&self, entity: &str, attr: &str) -> String {
        match self.attr(entity, attr) {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }

    /// Wait until the attribute of an entity has a value that `accept` takes.
    pub async fn wait_attr(
        &mut self,
        entity: &str,
        attr: &str,
        secs: u64,
        accept: impl Fn(&Value) -> bool,
    ) -> Result<Value, Fail> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = self.attr(entity, attr).filter(|v| accept(v)) {
                return Ok(v.clone());
            }
            if Instant::now() >= deadline {
                return Err(Fail::new(format!(
                    "{entity} {attr}: no expected value in {secs} s, last: {}",
                    self.attr(entity, attr)
                        .map_or("none".into(), Value::to_string)
                )));
            }
            self.pump(deadline.min(Instant::now() + Duration::from_millis(200)))
                .await?;
        }
    }

    pub async fn wait_text(
        &mut self,
        entity: &str,
        attr: &str,
        secs: u64,
        expected: &str,
    ) -> Result<(), Fail> {
        self.wait_attr(entity, attr, secs, |v| v.as_str() == Some(expected))
            .await
            .map(|_| ())
    }

    /// Read events for `secs`.
    pub async fn listen(&mut self, secs: u64) -> Result<(), Fail> {
        self.listen_ms(secs * 1000).await
    }

    pub async fn listen_ms(&mut self, ms: u64) -> Result<(), Fail> {
        self.listen_until(Instant::now() + Duration::from_millis(ms))
            .await
    }

    /// Read events until `deadline`.
    pub async fn listen_until(&mut self, deadline: Instant) -> Result<(), Fail> {
        while Instant::now() < deadline {
            self.pump(deadline).await?;
        }
        Ok(())
    }

    /// The next `driver_setup_change` event.
    pub async fn next_setup(&mut self, secs: u64) -> Result<Value, Fail> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(event) = self.setup.pop_front() {
                return Ok(event);
            }
            if Instant::now() >= deadline {
                return Err(Fail::new(format!("no setup event in {secs} s")));
            }
            self.pump(deadline).await?;
        }
    }

    /// The next setup page. Fails if the setup ends.
    pub async fn next_page(&mut self, secs: u64) -> Result<Value, Fail> {
        loop {
            let event = self.next_setup(secs).await?;
            if let Some(input) = event["require_user_action"]["input"].as_object() {
                return Ok(Value::Object(input.clone()));
            }
            if is_stop(&event) {
                return Err(Fail::new(format!("setup ended: {event}")));
            }
        }
    }

    /// The end of the setup. Fails if the state is not `OK`.
    pub async fn setup_done(&mut self, secs: u64) -> Result<(), Fail> {
        loop {
            let event = self.next_setup(secs).await?;
            if is_stop(&event) {
                return if event["state"] == "OK" {
                    Ok(())
                } else {
                    Err(Fail::new(format!("setup failed: {event}")))
                };
            }
        }
    }

    /// Forget the setup events that are not read yet. Returns how many there were.
    pub fn clear_setup(&mut self) -> usize {
        let count = self.setup.len();
        self.setup.clear();
        count
    }

    /// Answer the current setup page.
    pub async fn answer(&mut self, values: Value) -> Result<(), Fail> {
        self.request_ok("set_driver_user_data", json!({ "input_values": values }))
            .await
            .map(|_| ())
    }
}

/// A pong with `data`.
fn is_pong(frame: &Message, data: &[u8]) -> bool {
    matches!(frame, Message::Pong(got) if got == data)
}

/// Fails if `frame` does not have `code` and `reason`.
pub fn close_echo(frame: Option<&CloseFrame>, code: u16, reason: &str) -> Result<(), String> {
    match frame {
        None => Err("close frame without a code".to_owned()),
        Some(f) if u16::from(f.code) != code || f.reason != reason => Err(format!(
            "close frame {} {:?}, expected {code} {reason:?}",
            u16::from(f.code),
            f.reason.as_str()
        )),
        Some(_) => Ok(()),
    }
}

fn is_stop(event: &Value) -> bool {
    event["event_type"]
        .as_str()
        .is_some_and(|t| t.eq_ignore_ascii_case("stop"))
}

/// The ids of the fields of a setup page.
pub fn field_ids(page: &Value) -> Vec<String> {
    page["settings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect()
}

/// The value of a field of a setup page.
pub fn field_value(page: &Value, id: &str) -> Option<String> {
    let setting = page["settings"]
        .as_array()?
        .iter()
        .find(|s| s["id"] == id)?;
    let field = setting["field"].as_object()?.values().next()?;
    match &field["value"] {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_pong_with_the_same_data_matches() {
        assert!(is_pong(&Message::Pong(Bytes::from_static(&[3])), &[3]));
        assert!(!is_pong(&Message::Pong(Bytes::from_static(&[4])), &[3]));
        assert!(!is_pong(&Message::Pong(Bytes::new()), &[3]));
        assert!(!is_pong(&Message::Ping(Bytes::from_static(&[3])), &[3]));
    }

    #[test]
    fn close_echo_needs_the_same_code_and_reason() {
        let frame = |code: u16, reason: &'static str| CloseFrame {
            code: code.into(),
            reason: Utf8Bytes::from_static(reason),
        };
        assert_eq!(Ok(()), close_echo(Some(&frame(4001, "bye")), 4001, "bye"));
        assert_eq!(
            Err("close frame 1000 \"bye\", expected 4001 \"bye\"".to_owned()),
            close_echo(Some(&frame(1000, "bye")), 4001, "bye")
        );
        assert!(close_echo(Some(&frame(4001, "")), 4001, "bye").is_err());
        assert!(close_echo(None, 4001, "bye").is_err());
    }
}
