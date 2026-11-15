// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Actor for one receiver: connection, state updates and commands.

use crate::config_store::DeviceConfig;
use crate::device::commands::{self, Expected};
use crate::device::{
    Command, Connect, DeviceEvent, DeviceStatus, Disconnect, Stop, UpdateConfig, discovery,
};
use crate::entities::{self, EntityKind, SensorKind, View, remote, select};
use crate::errors::ServiceError;
use actix::prelude::*;
use denon_marantz::{
    Ask, Avr, Change, Error as AvrError, Query, Setting, Started, State, TelnetStatus,
};
use futures::StreamExt;
use log::{debug, info, warn};
use serde_json::Map;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};
use tokio::time::timeout;
use tokio_stream::wrappers::{BroadcastStream, WatchStream};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Longest wait for a running connect before a command runs. The core waits 10 s for the answer.
const CONNECT_WAIT: Duration = Duration::from_secs(6);
/// A connection that stayed up this long resets the reconnect delay.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// Changes come in bursts. Entity attributes are sent once after the last one.
const EMIT_DELAY: Duration = Duration::from_millis(50);
/// A new speaker preset or Dirac filter changes values that the receiver does not send again.
/// They are read after the burst of events is over.
const RELOAD_DELAY: Duration = Duration::from_millis(1500);
/// Reads over HTTP that fail in a row, while telnet is down, before the receiver is searched
/// again.
const POLL_FAILURES_BEFORE_RECONNECT: u32 = 3;
/// A command waits at most this long for the connect and for the commands before it. Nothing is
/// sent when the time is over.
const SEND_WAIT_LIMIT: Duration = Duration::from_secs(8);
/// A disconnect waits at most this long for a command that is running.
const DISCONNECT_WAIT: Duration = Duration::from_secs(2);
/// Time between reads of the state over HTTP, while telnet does not run. The Python integration
/// reads every 10 seconds too.
const HTTP_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// A state change of the receiver. The generation tells which connection sent it.
#[derive(Message)]
#[rtype(result = "()")]
struct Changed(u64, Option<Change>);

/// The link to the receiver went up or down.
#[derive(Message)]
#[rtype(result = "()")]
struct LinkChanged(u64, bool);

/// Result of a connect: the receiver, its changes and its address.
struct Connected {
    avr: Arc<Avr>,
    changes: broadcast::Receiver<Change>,
    /// The address, if it is not the one of the configuration.
    address: Option<String>,
    /// The start, which reads the web pages in the background.
    started: Started,
}

#[derive(Default)]
struct Timers {
    /// The running connect attempt.
    connect: Option<SpawnHandle>,
    reconnect: Option<SpawnHandle>,
    poll: Option<SpawnHandle>,
    signal: Option<SpawnHandle>,
}

#[allow(clippy::struct_excessive_bools)]
pub struct DenonActor {
    cfg: DeviceConfig,
    /// Incremented on each configuration change.
    cfg_version: u64,
    key: Arc<str>,
    controller: Recipient<DeviceEvent>,
    avr: Option<Arc<Avr>>,
    /// Last state of the receiver. Shown while there is no connection.
    last: State,
    /// Last address of the picture of what plays.
    last_artwork: Option<String>,
    /// The option of the active sound mode. The last one is kept while the receiver reports a
    /// mode that is not in its list: after a change, it reports another mode for about 1 s.
    sound_mode: Option<String>,
    /// What plays was reported after the last source change. The receiver keeps the values of the
    /// old source, so they are not shown before.
    status: DeviceStatus,
    /// Sends each status change. Commands wait on it while a connect runs.
    status_tx: watch::Sender<DeviceStatus>,
    /// The controller wants a connection.
    want: bool,
    /// The receiver answers: telnet is connected, or the last HTTP read worked.
    telnet_up: bool,
    /// The last read over HTTP worked.
    http_ok: bool,
    /// A read over HTTP is running.
    polling: bool,
    /// Reads over HTTP in a row that failed while telnet was down.
    poll_failures: u32,
    /// A read of the signal is running.
    signal_polling: bool,
    /// A read after a telnet reconnect is running.
    refreshing: bool,
    /// The connection was closed on request: the entities keep their state.
    keep_state: bool,
    attempts: u32,
    /// Incremented on each connect attempt and on close. Results of older attempts are ignored.
    attempt: u64,
    /// Incremented on each connect and disconnect. Results of older connections are ignored.
    generation: u64,
    connected_at: Option<Instant>,
    /// An emit is scheduled.
    emit_due: bool,
    /// All entities must be sent at the next emit.
    dirty_full: bool,
    /// The entities that show the changed settings, as bits of `part_bit`.
    dirty_part: u8,
    /// A read of the values that depend on the speaker preset and the filter is planned.
    reload_due: bool,
    /// The reasons to read values again, as bits of `RELOAD_QUERIES`.
    reload_parts: u8,
    /// Network audio was paused with the pause command. See `View::paused`.
    paused: bool,
    /// Time before a page or a toggle that failed is read again.
    page_retry_delay: Duration,
    /// Time between reads over HTTP while telnet is down.
    poll_interval: Duration,
    /// Commands run one after the other.
    cmd_lock: Arc<tokio::sync::Mutex<()>>,
    expected: Arc<Expected>,
    timers: Timers,
}

impl DenonActor {
    /// Set the time before a page or a toggle that failed is read again. For tests.
    #[cfg(test)]
    pub const fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.page_retry_delay = delay;
        self
    }

    /// Set the time between reads over HTTP. For tests.
    #[cfg(test)]
    pub const fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// `key` is shared with the controller, which uses it to tell this actor from an older one.
    pub fn new(key: Arc<str>, cfg: DeviceConfig, controller: Recipient<DeviceEvent>) -> Self {
        Self {
            key,
            cfg,
            cfg_version: 0,
            controller,
            avr: None,
            last: State::new(true),
            last_artwork: None,
            sound_mode: None,
            status: DeviceStatus::Disconnected,
            status_tx: watch::Sender::new(DeviceStatus::Disconnected),
            want: false,
            telnet_up: false,
            http_ok: false,
            polling: false,
            poll_failures: 0,
            signal_polling: false,
            refreshing: false,
            keep_state: false,
            attempts: 0,
            attempt: 0,
            generation: 0,
            connected_at: None,
            emit_due: false,
            dirty_full: true,
            dirty_part: 0,
            reload_due: false,
            reload_parts: 0,
            paused: false,
            page_retry_delay: discovery::PAGE_RETRY_DELAY,
            poll_interval: HTTP_POLL_INTERVAL,
            cmd_lock: Arc::default(),
            expected: Arc::default(),
            timers: Timers::default(),
        }
    }

    /// Send the attributes of all entities to the controller. It sends only the changed ones on.
    fn emit(&mut self) {
        self.dirty_full = false;
        self.dirty_part = 0;
        self.emit_kinds(|_| true);
    }

    /// Send the attributes of the entities that `keep` takes.
    fn emit_kinds(&mut self, keep: impl Fn(EntityKind) -> bool) {
        let connected = self.linked() || self.keep_state;
        let changes = if let Some(avr) = &self.avr {
            // A borrow of the state: no copy. It is not held longer than this call.
            let state = avr.state();
            let state = state.borrow();
            self.last_artwork = avr.artwork_url();
            if let Some(mode) = select::active_sound_mode(&state)
                && self.sound_mode.as_deref() != Some(mode)
            {
                self.sound_mode = Some(mode.to_owned());
            }
            build_changes(
                &state,
                &self.cfg,
                connected,
                self.last_artwork.as_deref(),
                self.paused,
                self.sound_mode.as_deref(),
                keep,
            )
        } else {
            build_changes(
                &self.last,
                &self.cfg,
                connected,
                self.last_artwork.as_deref(),
                self.paused,
                self.sound_mode.as_deref(),
                keep,
            )
        };
        self.controller.do_send(DeviceEvent::Changes {
            key: self.key.clone(),
            changes,
        });
    }

    /// Send what the changes since the last send can have changed. A volume, mute or title change
    /// builds only the entities that show them: this is most of the events.
    fn flush_emit(&mut self) {
        if self.dirty_full || self.dirty_part == 0 {
            self.emit();
            return;
        }
        let part = std::mem::take(&mut self.dirty_part);
        self.emit_kinds(|kind| part_bit(kind) & part != 0);
    }

    /// The receiver answers: telnet is connected, or the last read over HTTP worked.
    const fn linked(&self) -> bool {
        self.http_ok || (self.cfg.use_telnet && self.telnet_up)
    }

    /// Changes do not come as events: the state is read over HTTP.
    const fn needs_poll(&self) -> bool {
        !self.cfg.use_telnet || !self.telnet_up
    }

    /// Read single values again, after a change that makes the receiver change other values
    /// without sending them: the Audyssey values after a Dirac filter or a preset, the Dirac
    /// slots after a preset (they belong to the preset) and the digital codec after a new source.
    fn schedule_reload(&mut self, ctx: &mut Context<Self>, parts: u8) {
        self.reload_parts |= parts;
        if self.reload_due || !self.telnet_up {
            return;
        }
        self.reload_due = true;
        ctx.run_later(RELOAD_DELAY, |act, _| {
            act.reload_due = false;
            let parts = std::mem::take(&mut act.reload_parts);
            let Some(avr) = act.avr.clone().filter(|_| act.telnet_up) else {
                return;
            };
            actix::spawn(async move {
                let asks: Vec<Ask<'_>> = RELOAD_QUERIES
                    .iter()
                    .filter(|(bit, _)| parts & bit != 0)
                    .flat_map(|(_, queries)| queries.iter().copied().map(Ask::from))
                    .collect();
                if let Err(e) = avr.telnet_ask(&asks).await {
                    debug!("values not read: {e}");
                }
            });
        });
    }

    /// Send all entities soon.
    fn schedule_emit(&mut self, ctx: &mut Context<Self>) {
        self.dirty_full = true;
        self.arm_emit(ctx);
    }

    /// Send the entities of a changed setting soon.
    fn schedule_emit_for(&mut self, setting: Setting, ctx: &mut Context<Self>) {
        match setting_parts(setting) {
            Some(part) => self.dirty_part |= part,
            None => self.dirty_full = true,
        }
        self.arm_emit(ctx);
    }

    fn arm_emit(&mut self, ctx: &mut Context<Self>) {
        if !self.emit_due {
            self.emit_due = true;
            ctx.run_later(EMIT_DELAY, |act, _| {
                act.emit_due = false;
                act.flush_emit();
            });
        }
    }

    /// Status updates while the first connect after a connect request runs. Commands wait for
    /// it. A retry after a failure does not count: the receiver is not reachable, so a command
    /// fails at once and does not wait.
    fn running_connect(&self) -> Option<watch::Receiver<DeviceStatus>> {
        (self.avr.is_none()
            && self.attempts == 0
            && *self.status_tx.borrow() == DeviceStatus::Connecting)
            .then(|| self.status_tx.subscribe())
    }

    /// Set the status and show it, if the shown status is different.
    fn set_status(&mut self, status: DeviceStatus) {
        self.status = status;
        if *self.status_tx.borrow() != status {
            self.status_tx.send_replace(status);
            self.controller.do_send(DeviceEvent::Status {
                key: self.key.clone(),
                status,
            });
        }
    }

    /// Use a new configuration.
    fn set_config(&mut self, cfg: DeviceConfig) {
        self.cfg = cfg;
        self.emit();
    }

    fn connect(&mut self, ctx: &mut Context<Self>) {
        if !self.want
            || matches!(
                self.status,
                DeviceStatus::Connecting | DeviceStatus::Connected
            )
        {
            return;
        }
        self.set_status(DeviceStatus::Connecting);
        self.attempt += 1;
        let attempt = self.attempt;
        let cfg = self.cfg.clone();
        let version = self.cfg_version;
        let start = Instant::now();

        let fut = connect_avr(cfg);
        let handle = ctx.spawn(fut.into_actor(self).map(move |result, act, ctx| {
            act.timers.connect = None;
            if act.attempt != attempt {
                debug!(
                    "[{}] result of an old connect attempt ignored",
                    act.cfg.name
                );
                return;
            }
            match result {
                Ok(connected) if act.want && act.status == DeviceStatus::Connecting => {
                    act.on_connected(connected, version, start, ctx);
                }
                Ok(_) => {}
                Err(e) => {
                    match e {
                        Some(e) => warn!("[{}] connect failed: {e}", act.cfg.name),
                        None => info!("[{}] not found on the network", act.cfg.name),
                    }
                    act.lost_connection(ctx);
                    act.schedule_reconnect(ctx);
                }
            }
        }));
        self.timers.connect = Some(handle);
    }

    fn on_connected(
        &mut self,
        connected: Connected,
        version: u64,
        start: Instant,
        ctx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        let Connected {
            avr,
            changes,
            address,
            started,
        } = connected;
        info!(
            "[{}] connected in {} ms",
            self.cfg.name,
            start.elapsed().as_millis()
        );
        self.connected_at = Some(Instant::now());
        self.keep_state = false;
        self.http_ok = true;
        self.telnet_up =
            self.cfg.use_telnet && *avr.telnet_status().borrow() == TelnetStatus::Connected;
        // Values from before the connect are not valid anymore.
        self.expected.clear();
        self.paused = false;
        // The list of the receiver can be another one now.
        self.sound_mode = None;
        self.update_address(address, version);
        self.update_receiver_kind(&avr, version);

        ctx.add_message_stream(
            BroadcastStream::new(changes).map(move |change| Changed(generation, change.ok())),
        );
        if self.cfg.use_telnet {
            // With HTTP only, a page read could delay a command: the pages are not read again.
            actix::spawn(discovery::read_missing_pages(
                avr.clone(),
                started,
                self.cfg.name.clone(),
                self.page_retry_delay,
            ));
            let status = WatchStream::from_changes(avr.telnet_status())
                .map(move |s| LinkChanged(generation, s == TelnetStatus::Connected));
            ctx.add_message_stream(status);
        }
        // Without telnet events, the state is read over HTTP. This also covers a telnet connection
        // that is down.
        self.timers.poll = Some(ctx.run_interval(self.poll_interval, Self::poll_http));
        if self.cfg.signal_poll_secs > 0 {
            let every = Duration::from_secs(u64::from(self.cfg.signal_poll_secs));
            self.timers.signal = Some(ctx.run_interval(every, Self::poll_signal));
        }

        self.avr = Some(avr);
        self.set_status(DeviceStatus::Connected);
        self.emit();
    }

    /// Take what the configuration keeps of the receiver: the generation, the HTTP read method, the
    /// HDMI output over HTTP and the number of quick selects. A configuration of the Python
    /// integration does not have them. The controller saves them.
    fn update_receiver_kind(&mut self, avr: &Avr, version: u64) {
        let kind = discovery::receiver_kind(avr.info());
        if self.cfg_version != version || kind == self.cfg.kind() {
            return;
        }
        info!("[{}] receiver: {kind:?}", self.cfg.name);
        self.cfg.set_kind(kind);
        self.controller.do_send(DeviceEvent::Config {
            key: self.key.clone(),
            cfg: self.cfg.clone(),
        });
    }

    /// Take the address that was found by the search. The controller saves it.
    fn update_address(&mut self, address: Option<String>, version: u64) {
        let Some(address) = address else { return };
        if self.cfg_version != version || address == self.cfg.address {
            return;
        }
        info!(
            "[{}] address changed: {} -> {address}",
            self.cfg.name, self.cfg.address
        );
        self.cfg.address = address;
        self.controller.do_send(DeviceEvent::Config {
            key: self.key.clone(),
            cfg: self.cfg.clone(),
        });
    }

    /// Read the state over HTTP and show if the receiver answers. It does nothing while telnet
    /// reports the changes, or while an earlier read runs. In HTTP mode Audyssey, the signal and
    /// the HDMI output are read after the state: the library asks only receivers that use
    /// `AppCommand`. In telnet mode they are not: while telnet is down, a blank answer would clear
    /// the values of telnet.
    fn poll_http(&mut self, ctx: &mut Context<Self>) {
        if !self.needs_poll() || self.polling {
            return;
        }
        let Some(avr) = self.avr.clone() else { return };
        self.polling = true;
        let generation = self.generation;
        let settings = !self.cfg.use_telnet;
        ctx.spawn(
            async move {
                let result = avr.update().await;
                let extra = match &result {
                    Ok(()) if settings => Some(avr.update_signal_and_settings().await),
                    _ => None,
                };
                (result, extra)
            }
            .into_actor(self)
            .map(move |(result, extra), act, ctx| {
                // A result of an older connection must not clear the flag of the new one.
                if act.generation != generation {
                    return;
                }
                act.polling = false;
                if let Some(Err(e)) = &extra {
                    debug!("[{}] signal and settings not read: {e}", act.cfg.name);
                }
                let ok = result.is_ok();
                if let Err(e) = &result {
                    debug!("[{}] read over HTTP failed: {e}", act.cfg.name);
                }
                // Changes of the values come as events. The entities show the link too.
                let flipped = act.http_ok != ok;
                act.http_ok = ok;
                if ok {
                    act.poll_failures = 0;
                } else {
                    act.poll_failures += 1;
                    // The receiver can have a new address. Connect again: this searches for its
                    // serial number.
                    if act.poll_failures >= POLL_FAILURES_BEFORE_RECONNECT && act.want {
                        act.poll_failures = 0;
                        info!(
                            "[{}] the receiver does not answer, searching it again",
                            act.cfg.name
                        );
                        act.lost_connection(ctx);
                        act.attempts = 0;
                        act.connect(ctx);
                        return;
                    }
                }
                if flipped {
                    act.schedule_emit(ctx);
                }
            }),
        );
    }

    fn poll_signal(&mut self, ctx: &mut Context<Self>) {
        // A slow receiver must not get a new read before the old one is done.
        if self.signal_polling {
            return;
        }
        let Some(avr) = self.avr.clone() else { return };
        self.signal_polling = true;
        let generation = self.generation;
        ctx.spawn(
            async move {
                if let Err(e) = avr.refresh_signal().await {
                    debug!("signal not read: {e}");
                }
            }
            .into_actor(self)
            .map(move |(), act, _| {
                if act.generation == generation {
                    act.signal_polling = false;
                }
            }),
        );
    }

    /// Drop the connection and mark the entities unavailable.
    fn lost_connection(&mut self, ctx: &mut Context<Self>) {
        self.close(ctx);
        self.set_status(DeviceStatus::Disconnected);
        self.keep_state = false;
        self.emit();
    }

    /// Close the connection and stop all tasks of it.
    fn close(&mut self, ctx: &mut Context<Self>) {
        self.generation += 1;
        self.attempt += 1;
        // A connection that drops at once keeps the reconnect delay growing.
        if self
            .connected_at
            .take()
            .is_some_and(|t| t.elapsed() >= STABLE_CONNECTION)
        {
            self.attempts = 0;
        }
        self.telnet_up = false;
        self.http_ok = false;
        self.polling = false;
        self.signal_polling = false;
        self.refreshing = false;
        // An older connect attempt must not run on: it would read the receiver in parallel with
        // the next one.
        for handle in [
            self.timers.connect.take(),
            self.timers.poll.take(),
            self.timers.signal.take(),
        ]
        .into_iter()
        .flatten()
        {
            ctx.cancel_future(handle);
        }
        if let Some(avr) = self.avr.take() {
            // Keep the last state for the entities.
            self.last = avr.snapshot();
            actix::spawn(async move { avr.telnet_disconnect().await });
        }
    }

    fn cancel_timers(&mut self, ctx: &mut Context<Self>) {
        let t = std::mem::take(&mut self.timers);
        for handle in [t.connect, t.reconnect, t.poll, t.signal]
            .into_iter()
            .flatten()
        {
            ctx.cancel_future(handle);
        }
    }

    fn schedule_reconnect(&mut self, ctx: &mut Context<Self>) {
        if !self.want {
            return;
        }
        self.cancel_timers(ctx);
        self.attempts = self.attempts.saturating_add(1);
        let delay = Duration::from_secs(u64::from(self.attempts) * 2).min(MAX_BACKOFF);
        debug!("[{}] reconnect in {}s", self.cfg.name, delay.as_secs());
        self.timers.reconnect = Some(ctx.run_later(delay, |act, ctx| {
            act.timers.reconnect = None;
            act.connect(ctx);
        }));
    }

    /// A new source, a power change, or a title that plays again ends the pause.
    fn clear_pause(&mut self, setting: Setting) {
        if !self.paused {
            return;
        }
        let playing = self.avr.as_ref().is_some_and(|avr| {
            avr.state().borrow().device().now_playing.is_playing() == Some(true)
        });
        if ends_pause(setting, playing) {
            self.paused = false;
        }
    }

    /// Run the steps of a remote command in the background.
    fn spawn_run(
        &self,
        run: remote::RemoteRun,
        connecting: Option<watch::Receiver<DeviceStatus>>,
        ctx: &mut Context<Self>,
    ) {
        let lock = self.cmd_lock.clone();
        let cfg = self.cfg.clone();
        let expected = self.expected.clone();
        ctx.spawn(
            async move {
                connect_ended(connecting).await;
            }
            .into_actor(self)
            .then(move |(), act, _| {
                let avr = act.avr.clone();
                let name = act.cfg.name.clone();
                async move {
                    let Some(avr) = avr else {
                        warn!("[{name}] remote command: the receiver is not connected");
                        return;
                    };
                    match commands::run_steps(&avr, &cfg, &expected, &run, &lock).await {
                        Err(ServiceError::Blocked(reason)) => {
                            info!("[{name}] remote command ignored: {reason}");
                        }
                        Err(e) => warn!("[{name}] remote command failed: {e}"),
                        Ok(()) => {}
                    }
                }
                .into_actor(act)
            }),
        );
    }

    /// Start a connect if none runs.
    fn start_connect(&mut self, ctx: &mut Context<Self>) {
        self.want = true;
        if self.status == DeviceStatus::Error {
            self.set_status(DeviceStatus::Disconnected);
        }
        if self.status == DeviceStatus::Disconnected {
            if let Some(handle) = self.timers.reconnect.take() {
                ctx.cancel_future(handle);
            }
            self.attempts = 0;
            self.connect(ctx);
        }
    }
}

/// Find the receiver and start it. Retries network errors fast, which covers the short Wi-Fi
/// drop of the Remote after wake-up. `Err(None)` means the receiver is not on the network.
///
/// The stall timeout covers only the short identify step. The start is not canceled: a slow
/// receiver needs more time to read its values.
async fn connect_avr(cfg: DeviceConfig) -> Result<Connected, Option<AvrError>> {
    let deadline = Instant::now() + discovery::NETWORK_RETRY_TIMEOUT;
    let mut address = None;
    let mut found = None;
    let mut last_error = None;
    // The last good address first.
    if !cfg.address.is_empty() {
        match discovery::identify_retry(&cfg.address, deadline).await {
            Ok(avr) => found = Some(avr),
            Err(e) => {
                debug!("[{}] last address failed: {e}", cfg.name);
                last_error = Some(e);
            }
        }
    }
    // The address can have changed: search for the serial number.
    if found.is_none() {
        let receivers = discovery::discover().await.map_err(Some)?;
        let Some(receiver) = discovery::find(&cfg, &receivers) else {
            return Err(last_error);
        };
        let avr = discovery::identify_retry(&receiver.host, deadline)
            .await
            .map_err(Some)?;
        address = Some(receiver.host.clone());
        found = Some(avr);
    }
    let avr = Arc::new(found.ok_or(last_error)?);
    // Subscribe before the start, so that no change is missed.
    let changes = avr.changes();
    let started = discovery::start(&avr, cfg.use_telnet).await.map_err(Some)?;
    Ok(Connected {
        avr,
        changes,
        address,
        started,
    })
}

/// Wait until a running connect has ended, at most [`CONNECT_WAIT`].
async fn connect_ended(status: Option<watch::Receiver<DeviceStatus>>) {
    if let Some(mut status) = status {
        let ended = status.wait_for(|s| *s != DeviceStatus::Connecting);
        let _ = timeout(CONNECT_WAIT, ended).await;
    }
}

impl Actor for DenonActor {
    type Context = Context<Self>;

    fn started(&mut self, _: &mut Self::Context) {
        self.keep_state = false;
        self.emit();
    }

    fn stopped(&mut self, ctx: &mut Self::Context) {
        self.close(ctx);
    }
}

/// The entities that a volume, mute or title change can change, as bits.
const fn setting_parts(setting: Setting) -> Option<u8> {
    match setting {
        Setting::Volume => Some(PART_PLAYER | PART_VOLUME_DB | PART_VOLUME_ABSOLUTE),
        Setting::Muted => Some(PART_PLAYER | PART_MUTE),
        Setting::NowPlaying => Some(PART_PLAYER),
        _ => None,
    }
}

const PART_PLAYER: u8 = 1;
const PART_VOLUME_DB: u8 = 2;
const PART_VOLUME_ABSOLUTE: u8 = 4;
const PART_MUTE: u8 = 8;

/// The bit of an entity in `setting_parts`. Zero: the entity is in no part.
const fn part_bit(kind: EntityKind) -> u8 {
    match kind {
        EntityKind::MediaPlayer => PART_PLAYER,
        EntityKind::Sensor(SensorKind::VolumeDb) => PART_VOLUME_DB,
        EntityKind::Sensor(SensorKind::VolumeAbsolute) => PART_VOLUME_ABSOLUTE,
        EntityKind::Sensor(SensorKind::Mute) => PART_MUTE,
        _ => 0,
    }
}

/// The attributes of the entities that `keep` takes.
fn build_changes(
    state: &State,
    cfg: &DeviceConfig,
    connected: bool,
    artwork_url: Option<&str>,
    paused: bool,
    sound_mode: Option<&str>,
    keep: impl Fn(EntityKind) -> bool,
) -> Vec<(EntityKind, Map<String, serde_json::Value>)> {
    let view = View {
        state,
        cfg,
        connected,
        artwork_url,
        paused,
        sound_mode,
    };
    EntityKind::all(cfg)
        .filter(|kind| keep(*kind))
        .map(|kind| {
            let mut attrs = Map::new();
            entities::attributes(&view, kind, &mut attrs);
            (kind, attrs)
        })
        .collect()
}

/// A new source or a power change ends a pause. So does a title that plays again.
const fn ends_pause(setting: Setting, playing: bool) -> bool {
    match setting {
        Setting::Source | Setting::Power => true,
        Setting::NowPlaying => playing,
        _ => false,
    }
}

const RELOAD_PRESET: u8 = 1;
const RELOAD_FILTER: u8 = 2;
const RELOAD_DYNAMIC_EQ: u8 = 4;
const RELOAD_MULTI_EQ: u8 = 8;
const RELOAD_SOURCE: u8 = 16;
/// The values that the Dirac filter blocks or changes: tone and Audyssey.
const FILTER_VALUES: [Query; 7] = [
    Query::ToneControl,
    Query::Bass,
    Query::Treble,
    Query::MultEq,
    Query::DynamicEq,
    Query::ReferenceLevel,
    Query::DynamicVolume,
];
/// The values that a reload reads for each reason. The Dirac slots belong to the speaker preset.
const RELOAD_QUERIES: [(u8, &[Query]); 5] = [
    (
        RELOAD_PRESET,
        &[
            Query::DiracSlots,
            Query::DiracFilter,
            Query::ToneControl,
            Query::Bass,
            Query::Treble,
            Query::MultEq,
            Query::DynamicEq,
            Query::ReferenceLevel,
            Query::DynamicVolume,
        ],
    ),
    (RELOAD_FILTER, &FILTER_VALUES),
    // The reference level offset needs Dynamic EQ.
    (RELOAD_DYNAMIC_EQ, &[Query::ReferenceLevel]),
    (
        RELOAD_MULTI_EQ,
        &[
            Query::DynamicEq,
            Query::ReferenceLevel,
            Query::DynamicVolume,
        ],
    ),
    // The digital codec depends on the source.
    (RELOAD_SOURCE, &[Query::DigitalCodec]),
];

/// A setting whose change makes the receiver change other values without sending them. Returns
/// the reasons to read values again.
const fn reload_after(setting: Setting) -> Option<u8> {
    match setting {
        Setting::SpeakerPreset => Some(RELOAD_PRESET),
        Setting::DiracFilter => Some(RELOAD_FILTER),
        Setting::DynamicEq => Some(RELOAD_DYNAMIC_EQ),
        Setting::MultiEq => Some(RELOAD_MULTI_EQ),
        Setting::Source => Some(RELOAD_SOURCE),
        _ => None,
    }
}

impl Handler<Changed> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: Changed, ctx: &mut Self::Context) {
        if msg.0 != self.generation {
            return;
        }
        if let Some(change) = msg.1 {
            if let Some(parts) = reload_after(change.setting) {
                self.schedule_reload(ctx, parts);
            }
            self.clear_pause(change.setting);
            // The kept sound mode can be missing from a new list.
            if change.setting == Setting::SoundModes {
                self.sound_mode = None;
            }
            self.schedule_emit_for(change.setting, ctx);
        } else {
            // Changes were lost.
            self.schedule_emit(ctx);
        }
    }
}

impl Handler<LinkChanged> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: LinkChanged, ctx: &mut Self::Context) {
        if msg.0 != self.generation || self.telnet_up == msg.1 {
            return;
        }
        info!(
            "[{}] telnet {}",
            self.cfg.name,
            if msg.1 { "connected" } else { "disconnected" }
        );
        self.telnet_up = msg.1;
        // After a reconnect, the library asks for its own groups again. The driver reads what only
        // HTTP gives and its extra values: asking for all groups again would repeat the queries.
        if msg.1
            && !self.refreshing
            && let Some(avr) = self.avr.clone()
        {
            self.refreshing = true;
            let generation = self.generation;
            ctx.spawn(
                async move {
                    if let Err(e) = avr.update().await {
                        debug!("read after reconnect failed: {e}");
                    }
                    discovery::read_extra_values(&avr).await;
                }
                .into_actor(self)
                .map(move |(), act, _| {
                    if act.generation == generation {
                        act.refreshing = false;
                    }
                }),
            );
        }
        // While telnet is down, the state is read over HTTP.
        if !msg.1 {
            self.poll_http(ctx);
        }
        self.schedule_emit(ctx);
    }
}

impl Handler<Connect> for DenonActor {
    type Result = ();

    fn handle(&mut self, _: Connect, ctx: &mut Self::Context) {
        self.start_connect(ctx);
    }
}

impl Handler<Disconnect> for DenonActor {
    type Result = ();

    fn handle(&mut self, _: Disconnect, ctx: &mut Self::Context) {
        self.want = false;
        self.cancel_timers(ctx);
        // Asked for, for example at standby: the entities keep their state. Only a lost
        // connection makes them unavailable.
        self.keep_state = true;
        if self.avr.is_none() {
            self.close(ctx);
            self.set_status(DeviceStatus::Disconnected);
            return;
        }
        // A command that is running, for example power off before standby, is sent first.
        let lock = self.cmd_lock.clone();
        ctx.spawn(
            async move { timeout(DISCONNECT_WAIT, lock.lock_owned()).await.ok() }
                .into_actor(self)
                .map(|guard, act, ctx| {
                    drop(guard);
                    // A connect that was asked for meanwhile keeps the connection.
                    if !act.want {
                        act.close(ctx);
                        act.set_status(DeviceStatus::Disconnected);
                    }
                }),
        );
    }
}

impl Handler<Stop> for DenonActor {
    type Result = ();

    fn handle(&mut self, _: Stop, ctx: &mut Self::Context) {
        self.want = false;
        ctx.stop();
    }
}

impl Handler<UpdateConfig> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: UpdateConfig, ctx: &mut Self::Context) {
        let cfg = msg.0;
        let reconnect = cfg.address != self.cfg.address
            || cfg.use_telnet != self.cfg.use_telnet
            || cfg.signal_poll_secs != self.cfg.signal_poll_secs;
        self.set_config(cfg);
        self.cfg_version += 1;
        if reconnect && self.want {
            info!("[{}] configuration changed, reconnecting", self.cfg.name);
            self.lost_connection(ctx);
            self.attempts = 0;
            self.connect(ctx);
        }
    }
}

impl Handler<Command> for DenonActor {
    type Result = ResponseActFuture<Self, Result<(), ServiceError>>;

    fn handle(&mut self, msg: Command, ctx: &mut Self::Context) -> Self::Result {
        // The core can send a command before `exit_standby`. While a retry after a failed connect
        // is pending, the receiver is not reachable: the command fails at once.
        let backoff = self.want && self.timers.reconnect.is_some();
        if self.avr.is_none() && self.status == DeviceStatus::Disconnected && !backoff {
            debug!("[{}] command while disconnected, connecting", self.cfg.name);
            self.start_connect(ctx);
        }
        // A command can come right after the Remote wakes up, while the connect still runs.
        let connecting = self.running_connect();
        if self.avr.is_none() && connecting.is_none() {
            return Box::pin(fut::ready(Err(ServiceError::NotConnected)));
        }
        // A remote command with more than one step is answered at once, as the Python integration
        // does, and runs on its own. Its steps do not hold up the other commands.
        if msg.kind == EntityKind::Remote {
            match remote::command(&msg.cmd_id, msg.params.as_ref()) {
                Err(e) => return Box::pin(fut::ready(Err(e))),
                Ok(run) if run.steps() > 1 => {
                    self.spawn_run(run, connecting, ctx);
                    return Box::pin(fut::ready(Ok(())));
                }
                Ok(_) => {}
            }
        }
        let lock = self.cmd_lock.clone();
        let cfg = self.cfg.clone();
        let expected = self.expected.clone();
        // Network audio plays now: a pause command pauses it.
        let was_playing = self.avr.as_ref().is_some_and(|avr| {
            avr.state().borrow().device().now_playing.is_playing() == Some(true)
        });
        // The remote entity passes the media player commands on, as in the Python integration.
        let media_command = match msg.kind {
            EntityKind::MediaPlayer => Some(msg.cmd_id.clone()),
            EntityKind::Remote => msg
                .params
                .as_ref()
                .and_then(|p| p.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        };
        Box::pin(
            async move {
                // Nothing is sent while it waits: it can answer with a timeout and be tried again.
                timeout(SEND_WAIT_LIMIT, async {
                    connect_ended(connecting).await;
                    lock.lock_owned().await
                })
                .await
                .map_err(|_| ServiceError::Timeout)
            }
            .into_actor(self)
            .then(move |guard, act, _| {
                let avr = act.avr.clone();
                let name = act.cfg.name.clone();
                async move {
                    let _guard = guard?;
                    let Some(avr) = avr else {
                        return Err(ServiceError::NotConnected);
                    };
                    let result = commands::run(
                        &avr,
                        &cfg,
                        &expected,
                        msg.kind,
                        &msg.cmd_id,
                        msg.params.as_ref(),
                    )
                    .await;
                    match result {
                        // The state of the receiver does not allow it. The Remote shows an error
                        // banner for any other answer than OK that the user must close, and the
                        // Python integration did nothing here: the answer is OK, and the log says
                        // why nothing happened.
                        Err(ServiceError::Blocked(reason)) => {
                            info!("[{name}] {} ignored: {reason}", msg.cmd_id);
                            Ok(())
                        }
                        Err(e) => {
                            warn!("[{name}] {} failed: {e}", msg.cmd_id);
                            Err(e)
                        }
                        Ok(()) => Ok(()),
                    }
                }
                .into_actor(act)
            })
            .map(move |result, act, ctx| {
                // The receiver does not tell a pause from a stop: the pause is remembered here, as
                // the Python integration does.
                if result.is_ok()
                    && let Some(cmd) = media_command.as_deref()
                {
                    match cmd {
                        "play_pause" => act.paused = !act.paused && was_playing,
                        "stop" | "next" | "previous" | "off" | "toggle" => act.paused = false,
                        _ => {}
                    }
                    act.schedule_emit(ctx);
                }
                result
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> DeviceConfig {
        DeviceConfig {
            id: "SERIAL0000001".into(),
            name: "AVR".into(),
            address: "192.0.2.1".into(),
            support_sound_mode: true,
            show_all_inputs: false,
            use_telnet: true,
            update_audyssey: false,
            zone2: false,
            zone3: false,
            volume_step: 0.5,
            timeout: 2000,
            is_denon: true,
            is_dirac_supported: Some(true),
            signal_poll_secs: 0,
            receiver_type: None,
            update_method: None,
            hdmi_output_over_http: None,
            quick_select_count: None,
            listed_queries: None,
        }
    }

    fn test_state(lines: &[&str]) -> State {
        use denon_marantz::proto::telnet::Message;
        let mut state = State::new(true);
        for line in lines {
            let message = Message::parse(line).expect("message");
            state.apply_message(&message);
        }
        state
    }

    /// The entities whose attributes differ between two states.
    fn changed_kinds(before: &State, after: &State) -> Vec<EntityKind> {
        let cfg = test_cfg();
        let all = |state: &State| {
            let mode = select::active_sound_mode(state);
            build_changes(state, &cfg, true, None, false, mode, |_| true)
        };
        all(before)
            .into_iter()
            .zip(all(after))
            .filter(|((_, a), (_, b))| a != b)
            .map(|((kind, _), _)| kind)
            .collect()
    }

    /// A partial send must contain every entity that the change can change: a missed entity would
    /// show an old value.
    #[test]
    fn a_partial_send_has_all_the_entities_that_the_change_changes() {
        let base = [
            "PWON",
            "MV45",
            "MUOFF",
            "SINET",
            "NSE0Now Playing",
            "NSE1A Title",
        ];
        for (setting, change) in [
            (Setting::Volume, "MV46"),
            (Setting::Muted, "MUON"),
            (Setting::NowPlaying, "NSE1Another Title"),
            (Setting::NowPlaying, "NSE2An Artist"),
        ] {
            let before = test_state(&base);
            let mut lines = base.to_vec();
            lines.push(change);
            let after = test_state(&lines);
            let part = setting_parts(setting).expect("a partial setting");
            let changed = changed_kinds(&before, &after);
            assert!(!changed.is_empty(), "{change} changes nothing");
            for kind in changed {
                assert!(
                    part_bit(kind) & part != 0,
                    "{change} changes {kind:?}, which is not sent"
                );
            }
        }
    }

    #[test]
    fn other_settings_send_all_entities() {
        for setting in [
            Setting::Power,
            Setting::Source,
            Setting::SoundMode,
            Setting::Signal,
        ] {
            assert_eq!(None, setting_parts(setting), "{setting:?}");
        }
    }

    #[test]
    fn a_new_preset_reads_the_dirac_slots_too() {
        assert_eq!(Some(RELOAD_PRESET), reload_after(Setting::SpeakerPreset));
        let (_, queries) = RELOAD_QUERIES[0];
        assert!(queries.contains(&Query::DiracSlots));
        assert!(queries.contains(&Query::DiracFilter));
    }

    #[test]
    fn a_filter_change_does_not_read_the_filter() {
        assert_eq!(Some(RELOAD_FILTER), reload_after(Setting::DiracFilter));
        assert!(!FILTER_VALUES.contains(&Query::DiracFilter));
    }

    #[test]
    fn an_audyssey_change_reads_the_values_that_depend_on_it() {
        assert_eq!(Some(RELOAD_DYNAMIC_EQ), reload_after(Setting::DynamicEq));
        assert_eq!(Some(RELOAD_MULTI_EQ), reload_after(Setting::MultiEq));
    }

    #[test]
    fn a_new_source_reads_the_digital_codec() {
        assert_eq!(Some(RELOAD_SOURCE), reload_after(Setting::Source));
    }

    #[test]
    fn every_reason_has_queries() {
        for (bit, queries) in RELOAD_QUERIES {
            assert_eq!(1, bit.count_ones());
            assert_ne!(0, queries.len());
        }
    }

    #[test]
    fn a_new_source_or_a_power_change_ends_the_pause() {
        assert!(ends_pause(Setting::Source, false));
        assert!(ends_pause(Setting::Power, false));
    }

    #[test]
    fn a_title_that_plays_again_ends_the_pause() {
        assert!(ends_pause(Setting::NowPlaying, true));
        assert!(!ends_pause(Setting::NowPlaying, false));
        assert!(!ends_pause(Setting::Volume, true));
    }

    #[test]
    fn other_settings_are_not_read_again() {
        for setting in [Setting::Volume, Setting::Dimmer, Setting::NowPlaying] {
            assert_eq!(None, reload_after(setting), "{setting:?}");
        }
    }
}
