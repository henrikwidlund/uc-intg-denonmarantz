// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Actor for one receiver: connection, state updates and commands.

use crate::config_store::DeviceConfig;
use crate::device::commands::{self, Expected};
use crate::device::{
    Command, Connect, DeviceEvent, DeviceStatus, Disconnect, Stop, UpdateConfig, discovery, heos,
};
use crate::entities::{
    self, EntityKind, SelectKind, SensorKind, View, media_player, remote, select,
};
use crate::errors::ServiceError;
use actix::WeakAddr;
use actix::prelude::*;
use denon_marantz::HeosPlayerStatus;
use denon_marantz::heos::{Change as HeosChange, PlayState, PlayerState};
use denon_marantz::{
    Ask, Avr, Change, Error as AvrError, Info, MissingSources, Query, Setting, StartStopper,
    Started, State, TelnetStatus,
};
use futures::StreamExt;
use log::{debug, info, warn};
use serde_json::Map;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};
use tokio::time::{sleep_until, timeout, timeout_at};
use tokio_stream::wrappers::{BroadcastStream, WatchStream};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that stayed up this long resets the reconnect delay.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// Changes come in bursts. Entity attributes are sent once after the last one.
const EMIT_DELAY: Duration = Duration::from_millis(50);
/// The wait after the last answer with a name before the names are saved.
const NAMES_SAVE_DELAY: Duration = Duration::from_secs(1);
/// A new speaker preset or Dirac filter changes values that the receiver does not send again.
/// They are read after the burst of events is over.
pub(super) const RELOAD_DELAY: Duration = Duration::from_millis(1500);
/// Reads over HTTP that fail in a row, while telnet is down, before the receiver is searched
/// again.
const POLL_FAILURES_BEFORE_RECONNECT: u32 = 3;
/// The wait of a command for the connect and for the commands before it ends this long before
/// the deadline of the command, so that the send has this time. Nothing is sent when the wait
/// ends this way.
const SEND_TIME: Duration = Duration::from_secs(1);
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

/// The answers of the start are in. The generation tells which connection sent it.
#[derive(Message)]
#[rtype(result = "()")]
struct StartSynced(u64);

/// A running connect has a receiver that takes commands, before its start ends. It has the
/// attempt of the connect.
#[derive(Message)]
#[rtype(result = "()")]
struct Early(u64, Arc<Avr>);

/// Asks for what a command needs to be sent again, see [`send_again`]. The answer is `None` after a
/// [`Disconnect`], a reconnect after a setup change, or a [`Stop`].
#[derive(Message)]
#[rtype(result = "Option<Again>")]
struct SendAgain {
    /// See [`Resend::without_state`].
    without_state: bool,
    /// See [`Resend::disconnects`].
    disconnects: u64,
}

/// Where a command goes now.
struct Target {
    avr: Option<Arc<Avr>>,
    heos: Option<heos::Link>,
    /// Set when the command runs before the start ends, see [`send_command`].
    attempt: Option<watch::Receiver<u64>>,
}

/// A running connect that a command waits for, see [`DenonActor::running_connect`].
struct Connecting {
    status: watch::Receiver<DeviceStatus>,
    /// Changes when the connection is closed or another connect starts: the wait then ends.
    attempt: watch::Receiver<u64>,
}

/// The answer to [`SendAgain`].
struct Again {
    target: Target,
    /// A running connect, if the command must wait for it.
    connecting: Option<Connecting>,
    /// The receiver before the start ends, for a command that does not read the state.
    early: Option<watch::Receiver<Option<Arc<Avr>>>>,
}

/// The start found other receiver values than the stored ones. It has the generation of the
/// connection and the configuration version of the connect.
#[derive(Message)]
#[rtype(result = "()")]
struct InfoChanged(u64, u64, Info);

/// A change of the HEOS players. The generation tells which connection sent it. `None` if changes
/// were lost.
#[derive(Message)]
#[rtype(result = "()")]
struct HeosChanged(u64, Option<HeosChange>);

/// The HEOS player of the receiver is ready, or not anymore. The generation tells which connection
/// sent it.
#[derive(Message)]
#[rtype(result = "()")]
struct HeosStatusChanged(u64, HeosPlayerStatus);

/// A position that the media player shows, and when it was taken.
#[derive(Debug, Clone, Copy)]
struct HeosPosition {
    shown: media_player::Position,
    playing: bool,
    at: Instant,
}

impl HeosPosition {
    /// The position now, if it counts on from `at`.
    fn expected(&self) -> u64 {
        if self.playing {
            self.shown.position + self.at.elapsed().as_secs()
        } else {
            self.shown.position
        }
    }
}

/// The shown position must change: it jumped, or the play state or the length changed.
fn position_moved(old: Option<&HeosPosition>, new: Option<&HeosPosition>) -> bool {
    match (old, new) {
        (None, None) => false,
        (Some(old), Some(new)) => {
            old.playing != new.playing
                || old.shown.duration != new.shown.duration
                || old.expected().abs_diff(new.shown.position) >= POSITION_JUMP
        }
        _ => true,
    }
}

/// A position that differs this much from the expected one is sent. HEOS reports the position
/// every few seconds: a smaller difference is the Remote's own count.
const POSITION_JUMP: u64 = 2;

/// The receiver of the current connection, for the tests.
#[cfg(test)]
#[derive(Message)]
#[rtype(result = "Option<std::sync::Weak<Avr>>")]
pub struct CurrentAvr;

/// True while the read after a telnet reconnect runs, for the tests.
#[cfg(test)]
#[derive(Message)]
#[rtype(result = "bool")]
pub struct Refreshing;

/// Holds the command lock for this time, like a command that runs, for the tests. The lock must
/// be free.
#[cfg(test)]
#[derive(Message)]
#[rtype(result = "()")]
pub struct HoldCommandLock(pub Duration);

/// Asks if the command lock is free, for the tests.
#[cfg(test)]
#[derive(Message)]
#[rtype(result = "bool")]
pub struct CommandLockFree;

/// Asks for [`WantState`], for the tests.
#[cfg(test)]
#[derive(Message)]
#[rtype(result = "WantState")]
pub struct Wants;

/// The connection state of the actor, for the tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct WantState {
    /// The controller wants a connection.
    pub want: bool,
    /// A reconnect is planned.
    pub reconnect: bool,
    /// The status of the actor.
    pub status: DeviceStatus,
    /// The entities keep their state while not connected.
    pub keep_state: bool,
}

/// Result of a connect: the receiver, its changes and its address.
struct Connected {
    avr: Arc<Avr>,
    changes: broadcast::Receiver<Change>,
    /// The address, if it is not the one of the configuration.
    address: Option<String>,
    /// The start, which reads the web pages in the background. Stop it if the result is not used:
    /// dropping it does not stop the start task.
    started: Started,
    /// Time of the try of the start that worked.
    took: Duration,
}

#[derive(Default)]
struct Timers {
    /// The running connect attempt.
    connect: Option<SpawnHandle>,
    reconnect: Option<SpawnHandle>,
    poll: Option<SpawnHandle>,
    /// The running read of `poll_http`. Cancelling it drops its copy of the old `Avr`.
    poll_read: Option<SpawnHandle>,
    signal: Option<SpawnHandle>,
    /// The running read of `poll_signal`. Cancelling it drops its copy of the old `Avr`.
    signal_read: Option<SpawnHandle>,
    /// The running read after a telnet reconnect. Cancelling it drops its copy of the old `Avr`.
    refresh: Option<SpawnHandle>,
    /// The reads after a connect: missing web pages and toggles. Cancelling it drops its copy of
    /// the old `Avr`.
    pages: Option<SpawnHandle>,
    /// The telnet connects after the start could not open telnet.
    telnet: Option<SpawnHandle>,
    /// The HEOS connects after the first one failed.
    heos: Option<SpawnHandle>,
    /// The save of the names of the tuner presets and network favorites, after their answers stop.
    names: Option<SpawnHandle>,
}

#[allow(clippy::struct_excessive_bools)]
pub struct DenonActor {
    cfg: DeviceConfig,
    /// Incremented on each `UpdateConfig`.
    cfg_version: u64,
    key: Arc<str>,
    controller: Recipient<DeviceEvent>,
    avr: Option<Arc<Avr>>,
    /// The HEOS player, while the library has found it. The `Avr` owns the connection: each new
    /// connection to the receiver opens HEOS again.
    heos: Option<heos::Link>,
    /// Incremented when HEOS is dropped. Changes of an older connection are ignored.
    heos_generation: u64,
    /// The position that the media player shows. See `View::position`.
    heos_position: Option<HeosPosition>,
    /// Last state of the receiver. Shown while there is no connection.
    last: State,
    /// Last address of the picture of what plays.
    last_artwork: Option<String>,
    /// The option of the active sound mode. The last one is kept while the receiver reports a
    /// mode that is not in its list: after a change, it reports another mode for about 1 s.
    sound_mode: Option<String>,
    /// The sound mode groups are the choices while the receiver gave no list. See
    /// `View::sound_groups`.
    sound_groups: bool,
    /// The connection status.
    status: DeviceStatus,
    /// Sends each status change. Commands wait on it while a connect runs.
    status_tx: watch::Sender<DeviceStatus>,
    /// The receiver of a running connect, before its start ends. Commands that do not read the
    /// state use it and wait on it.
    early_tx: watch::Sender<Option<Arc<Avr>>>,
    /// The controller or a command wants a connection. While connected, it is false only in the
    /// wait of a [`Disconnect`].
    want: bool,
    /// The receiver answers: telnet is connected, or the last HTTP read worked.
    telnet_up: bool,
    /// The last read over HTTP worked.
    http_ok: bool,
    /// A read over HTTP worked since the connect or since telnet went down. Used in telnet mode:
    /// `http_ok` is not updated while telnet is up.
    http_checked: bool,
    /// A read over HTTP is running.
    polling: bool,
    /// Reads over HTTP in a row that failed while telnet was down.
    poll_failures: u32,
    /// A read of the signal is running.
    signal_polling: bool,
    /// A read after a telnet reconnect is running.
    refreshing: bool,
    /// The names that were asked on this telnet connection, as bits of `Names::bit`.
    names_asked: u8,
    /// The names that changed and are not saved yet, as bits of `Names::bit`.
    names_pending: u8,
    /// The connection was closed on request: the entities keep their state.
    keep_state: bool,
    attempts: u32,
    /// The connects look the receiver up and do not use the stored values, until one succeeds.
    look_up: bool,
    /// Incremented on each connect attempt and on close. Results of older attempts are ignored,
    /// commands sent before the start ends stop when it changes, and the waits for a connect end,
    /// see [`Connecting`] and [`stale`].
    attempt: watch::Sender<u64>,
    /// The address of this actor, set when it starts. A weak address costs no allocation per
    /// command.
    me: Option<WeakAddr<Self>>,
    /// Incremented on each [`Disconnect`] and on each reconnect after a setup change. A command is
    /// not sent again after one, see also [`stale`].
    disconnects: u64,
    /// Set when a setup change needs a new connection during the wait of a [`Disconnect`]. A
    /// connect or command in the wait then connects again with the new values.
    stale_config: bool,
    /// How long the last connects took. They set the time limit of the connect tries, see
    /// [`discovery::Stall`].
    connect_times: discovery::ConnectTimes,
    /// Incremented on each connect and disconnect. Results of older connections are ignored.
    generation: u64,
    connected_at: Option<Instant>,
    /// An emit is scheduled.
    emit_due: bool,
    /// All entities must be sent at the next emit.
    dirty_full: bool,
    /// The entities that show the changed settings, as bits of `part_bit`.
    dirty_part: u8,
    /// The options of the network favorite and tuner preset selects must be built at the next
    /// emit. Without it, these selects give only their current option: they have up to 56 options.
    dirty_labels: bool,
    /// A read of the values that depend on the speaker preset and the filter is planned.
    reload_due: bool,
    /// The reasons to read values again, as bits of `RELOAD_QUERIES`.
    reload_parts: u8,
    /// Network audio was paused with the pause command. See `View::paused`.
    paused: bool,
    /// Time before a page or a telnet query that failed is tried again.
    page_retry_delay: Duration,
    /// Time between reads over HTTP while telnet is down.
    poll_interval: Duration,
    /// Time between signal reads in place of the one from the configuration. For tests.
    signal_interval: Option<Duration>,
    /// Commands run one after the other.
    cmd_lock: Arc<tokio::sync::Mutex<()>>,
    /// Gets a new value on `stop_send`. A remote run stops when it gets one after its command was
    /// received.
    run_stop: watch::Sender<()>,
    expected: Arc<Expected>,
    timers: Timers,
    /// Stops the start task of the library. Dropping `Started` does not stop it, and it holds
    /// the `Avr`.
    start_stopper: Option<StartStopper>,
}

impl DenonActor {
    /// Set the time before a page or a telnet query that failed is tried again. For tests.
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

    /// Set the time between signal reads. For tests.
    #[cfg(test)]
    pub const fn with_signal_interval(mut self, interval: Duration) -> Self {
        self.signal_interval = Some(interval);
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
            heos: None,
            heos_generation: 0,
            heos_position: None,
            last: State::new(true),
            last_artwork: None,
            sound_mode: None,
            sound_groups: true,
            status: DeviceStatus::Disconnected,
            status_tx: watch::Sender::new(DeviceStatus::Disconnected),
            early_tx: watch::Sender::new(None),
            want: false,
            telnet_up: false,
            http_ok: false,
            http_checked: false,
            polling: false,
            poll_failures: 0,
            signal_polling: false,
            refreshing: false,
            names_asked: 0,
            names_pending: 0,
            keep_state: false,
            attempts: 0,
            look_up: false,
            attempt: watch::Sender::new(0),
            me: None,
            disconnects: 0,
            stale_config: false,
            connect_times: discovery::ConnectTimes::default(),
            generation: 0,
            connected_at: None,
            emit_due: false,
            dirty_full: true,
            dirty_part: 0,
            dirty_labels: true,
            reload_due: false,
            reload_parts: 0,
            paused: false,
            page_retry_delay: discovery::PAGE_RETRY_DELAY,
            poll_interval: HTTP_POLL_INTERVAL,
            signal_interval: None,
            cmd_lock: Arc::default(),
            run_stop: watch::Sender::new(()),
            expected: Arc::default(),
            timers: Timers::default(),
            start_stopper: None,
        }
    }

    /// Send the attributes of all entities to the controller. It sends only the changed ones on.
    fn emit(&mut self) {
        self.dirty_labels = true;
        self.emit_changed();
    }

    /// Send the attributes of all entities. The selects with many options give only their current
    /// option, unless their options changed.
    fn emit_changed(&mut self) {
        self.dirty_full = false;
        self.dirty_part = 0;
        let labels = std::mem::take(&mut self.dirty_labels);
        self.emit_kinds(|_| true, labels);
    }

    /// Send the attributes of the entities that `keep` takes. Without `labels`, the selects with
    /// many options give only their current option.
    fn emit_kinds(&mut self, keep: impl Fn(EntityKind) -> bool, labels: bool) {
        if let Some(avr) = &self.avr {
            self.last_artwork = avr.artwork_url();
            if let Some(mode) = select::active_sound_mode(&avr.state().borrow(), self.sound_groups)
                && self.sound_mode.as_deref() != Some(mode)
            {
                self.sound_mode = Some(mode.to_owned());
            }
        }
        let player = |kind| kind == EntityKind::MediaPlayer;
        let mut changes = self.build(None, |kind| !player(kind) && keep(kind), labels);
        if keep(EntityKind::MediaPlayer) {
            // Only the media player reads HEOS: the HEOS state is locked while it is built.
            let shown = self
                .heos
                .as_ref()
                .and_then(|link| link.with_player(|heos| self.build(Some(heos), player, labels)))
                .unwrap_or_else(|| self.build(None, player, labels));
            changes.splice(0..0, shown);
        }
        self.controller.do_send(DeviceEvent::Changes {
            key: self.key.clone(),
            changes,
        });
    }

    /// The attributes of the entities that `keep` takes.
    fn build(
        &self,
        heos: Option<&PlayerState>,
        keep: impl Fn(EntityKind) -> bool,
        labels: bool,
    ) -> Vec<(EntityKind, Map<String, serde_json::Value>)> {
        let connected = self.linked() || self.keep_state;
        let artwork = self.last_artwork.as_deref();
        let sound_mode = self.sound_mode.as_deref();
        if let Some(avr) = &self.avr {
            // A borrow of the state: no copy. It is not held longer than this call.
            let state = avr.state();
            let state = state.borrow();
            let view = View {
                state: &state,
                cfg: &self.cfg,
                connected,
                artwork_url: artwork,
                paused: self.paused,
                sound_mode,
                sound_groups: self.sound_groups,
                heos,
                position: self.heos_position.map(|p| p.shown),
            };
            build_changes(&view, keep, labels)
        } else {
            let view = View {
                state: &self.last,
                cfg: &self.cfg,
                connected,
                artwork_url: artwork,
                paused: self.paused,
                sound_mode,
                sound_groups: self.sound_groups,
                heos: None,
                position: None,
            };
            build_changes(&view, keep, labels)
        }
    }

    /// Send what the changes since the last send can have changed. A volume, mute or title change
    /// builds only the entities that show them: this is most of the events.
    fn flush_emit(&mut self) {
        if self.dirty_full || self.dirty_part == 0 {
            self.emit_changed();
            return;
        }
        let part = std::mem::take(&mut self.dirty_part);
        self.emit_kinds(|kind| part_bit(kind) & part != 0, false);
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
                    .flat_map(|(_, queries)| queries.iter().copied())
                    .filter(|&query| avr.info().offers(query))
                    .map(Ask::from)
                    .collect();
                // An empty ask clears the filter of repeated events in the library.
                if asks.is_empty() {
                    return;
                }
                if let Err(e) = avr.telnet_ask(&asks).await {
                    debug!("values not read: {e}");
                }
            });
        });
    }

    /// Send all entities soon.
    fn schedule_emit(&mut self, ctx: &mut Context<Self>) {
        self.dirty_full = true;
        self.dirty_labels = true;
        self.arm_emit(ctx);
    }

    /// Send the entities of a changed setting soon.
    fn schedule_emit_for(&mut self, change: Change, ctx: &mut Context<Self>) {
        match setting_parts(change.setting) {
            Some(part) => self.dirty_part |= part,
            None => self.dirty_full = true,
        }
        // The selects with many options have no options while the main zone is off. New names
        // come with `save_names`.
        if change.setting == Setting::Power && is_main(change.zone) {
            self.dirty_labels = true;
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

    /// The status and the attempt of the first connect after a connect request, while it runs.
    /// Commands wait for it. A retry after a failure does not count: the receiver is not reachable,
    /// so a command fails at once and does not wait.
    fn running_connect(&self) -> Option<Connecting> {
        (self.avr.is_none()
            && self.attempts == 0
            && *self.status_tx.borrow() == DeviceStatus::Connecting)
            .then(|| Connecting {
                status: self.status_tx.subscribe(),
                attempt: self.attempt.subscribe(),
            })
    }

    /// See [`stale`].
    fn stale(&self, attempt: Option<&watch::Receiver<u64>>, disconnects: u64) -> bool {
        stale(attempt, disconnects, self.disconnects)
    }

    /// Set the status and show it, if the shown status is different.
    fn set_status(&mut self, status: DeviceStatus) {
        self.status = status;
        if *self.status_tx.borrow() != status {
            let old = self.status_tx.send_replace(status);
            debug!("[{}] status {old:?} -> {status:?}", self.cfg.name);
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
        self.attempt.send_modify(|a| *a += 1);
        let attempt = *self.attempt.borrow();
        // Values from before the connect are not valid anymore. Commands can run before the start
        // ends, so they are cleared now.
        self.expected.clear();
        let cfg = self.cfg.clone();
        let version = self.cfg_version;
        let start = Instant::now();
        let known = (!self.look_up && !self.cfg.address.is_empty())
            .then(|| self.cfg.info.clone())
            .flatten();

        let addr = ctx.address();
        let last = self.connect_times.fastest();
        let fut = connect_avr(cfg, known, last, move |avr| {
            addr.do_send(Early(attempt, avr));
        });
        let handle = ctx.spawn(fut.into_actor(self).map(move |result, act, ctx| {
            act.timers.connect = None;
            if *act.attempt.borrow() != attempt {
                debug!(
                    "[{}] result of an old connect attempt ignored",
                    act.cfg.name
                );
                if let Ok(connected) = result {
                    connected.started.stop();
                }
                return;
            }
            match result {
                Ok(connected) if act.want && act.status == DeviceStatus::Connecting => {
                    act.on_connected(connected, version, start, ctx);
                }
                Ok(connected) => connected.started.stop(),
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

    /// Stop a running connect.
    fn cancel_connect(&mut self, ctx: &mut Context<Self>) {
        if let Some(handle) = self.timers.connect.take() {
            ctx.cancel_future(handle);
            debug!("[{}] connect canceled", self.cfg.name);
        }
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
            took,
        } = connected;
        info!(
            "[{}] connected in {} ms",
            self.cfg.name,
            start.elapsed().as_millis()
        );
        self.connected_at = Some(Instant::now());
        self.look_up = false;
        self.keep_state = false;
        self.http_ok = true;
        self.http_checked = true;
        self.telnet_up =
            self.cfg.use_telnet && *avr.telnet_status().borrow() == TelnetStatus::Connected;
        self.names_asked = 0;
        self.early_tx.send_replace(None);
        self.paused = false;
        // The list of the receiver can be another one now. With telnet, the list can still come.
        self.sound_mode = None;
        self.sound_groups = !self.cfg.use_telnet;
        let mut changed = self.update_address(address, version);
        // After `update_address`: `took` is a time of the new address.
        self.connect_times.add(took);
        changed |= self.update_receiver_info(avr.info(), version);
        // One save for the address and the values.
        if changed {
            self.send_config();
        }

        ctx.add_message_stream(
            BroadcastStream::new(changes).map(move |change| Changed(generation, change.ok())),
        );
        let addr = ctx.address();
        if let Some(old) = self.start_stopper.replace(started.stopper()) {
            old.stop();
        }
        self.timers.pages = Some(
            ctx.spawn(
                discovery::after_start(
                    avr.clone(),
                    started,
                    self.cfg.use_telnet,
                    self.cfg.name.clone(),
                    self.page_retry_delay,
                    {
                        let addr = addr.clone();
                        move |info| addr.do_send(InfoChanged(generation, version, info))
                    },
                    move || addr.do_send(StartSynced(generation)),
                )
                .into_actor(self)
                .map(move |(), act, _| {
                    if act.generation == generation {
                        act.note_missing();
                    }
                }),
            ),
        );
        if self.cfg.use_telnet {
            let status = WatchStream::from_changes(avr.telnet_status())
                .map(move |s| LinkChanged(generation, s == TelnetStatus::Connected));
            ctx.add_message_stream(status);
            if !self.telnet_up {
                let fast_until = start + discovery::NETWORK_RETRY_TIMEOUT;
                let keep = discovery::keep_telnet(avr.clone(), fast_until, self.cfg.name.clone());
                self.timers.telnet = Some(ctx.spawn(keep.into_actor(self)));
            }
        }
        // Without telnet events, the state is read over HTTP. This also covers a telnet connection
        // that is down.
        self.timers.poll = Some(ctx.run_interval(self.poll_interval, Self::poll_http));
        if let Some(every) = self.signal_every() {
            // Half a poll time after the reads of the state, so that their requests do not wait
            // for each other. `signal_every` is a multiple of the poll time.
            self.timers.signal = Some(ctx.run_later(self.poll_interval / 2, move |act, ctx| {
                if act.generation == generation {
                    act.timers.signal = Some(ctx.run_interval(every, Self::poll_signal));
                }
            }));
        }

        self.avr = Some(avr);
        self.set_status(DeviceStatus::Connected);
        self.emit();
        self.watch_heos(ctx);
    }

    /// Follow the HEOS player of the receiver, if it has one (`CommApiVers` 0301 or higher). The
    /// start of the library opens HEOS and finds the player. Here a failed first connect is tried
    /// again.
    fn watch_heos(&mut self, ctx: &mut Context<Self>) {
        let Some(avr) = &self.avr else { return };
        if !avr.info().has_heos() {
            return;
        }
        let generation = self.heos_generation;
        let status = WatchStream::new(avr.watch_heos_status())
            .map(move |status| HeosStatusChanged(generation, status));
        ctx.add_message_stream(status);
        // A lost change can be about this player.
        let changes = BroadcastStream::new(avr.heos_changes())
            .map(move |change| HeosChanged(generation, change.ok()));
        ctx.add_message_stream(changes);
        let keep = heos::keep_heos(Arc::clone(avr), self.cfg.name.clone());
        self.timers.heos = Some(ctx.spawn(keep.into_actor(self)));
    }

    /// Stop following HEOS. The `Avr` closes the connection. A command that runs keeps its copy
    /// of the player until it ends.
    fn drop_heos(&mut self, ctx: &mut Context<Self>) {
        if let Some(handle) = self.timers.heos.take() {
            ctx.cancel_future(handle);
        }
        self.heos_generation += 1;
        self.heos = None;
        self.heos_position = None;
    }

    /// Show the HEOS player, or stop showing it.
    fn set_heos(&mut self, link: Option<heos::Link>, ctx: &mut Context<Self>) {
        if link.is_none() && self.heos.is_none() {
            return;
        }
        self.heos = link;
        self.heos_position = None;
        self.update_position();
        self.dirty_part |= PART_PLAYER;
        self.arm_emit(ctx);
    }

    /// Take the HEOS position if it jumped, or if the play state or the length changed. Returns
    /// true if the shown position changed.
    fn update_position(&mut self) -> bool {
        let now = self.heos.as_ref().and_then(|link| {
            link.with_player(|player| {
                let (progress, at) = (player.progress?, player.progress_at?);
                let playing = player.play_state == Some(PlayState::Play);
                let since = if playing { at.elapsed().as_millis() } else { 0 };
                let position = u64::try_from(u128::from(progress.position_ms) + since)
                    .unwrap_or(u64::MAX)
                    / 1000;
                Some(HeosPosition {
                    shown: media_player::Position {
                        position,
                        duration: progress.duration_ms / 1000,
                    },
                    playing,
                    at: Instant::now(),
                })
            })
            .flatten()
        });
        let changed = position_moved(self.heos_position.as_ref(), now.as_ref());
        if changed {
            self.heos_position = now;
        }
        changed
    }

    /// The answer to a command that needs no receiver. A stop ends the remote runs that were
    /// received before it. A command that waited in the mailbox until its time is over does
    /// nothing, also no connect. A remote command with more steps can run longer once it has
    /// started.
    fn answer_at_once(&self, msg: &Command) -> Option<Result<(), ServiceError>> {
        if msg.kind == EntityKind::Remote && remote::is_stop(&msg.cmd_id) {
            self.run_stop.send_replace(());
            return Some(Ok(()));
        }
        if tokio::time::Instant::now() >= msg.deadline {
            let name = &self.cfg.name;
            info!("[{name}] {} not sent: no time left", command_name(msg));
            return Some(Err(ServiceError::Timeout));
        }
        None
    }

    /// Run the actions of [`commands::is_heos_action`] on HEOS while the media player shows HEOS.
    /// They do not wait for the commands of the receiver: HEOS is another connection. `None` if the
    /// command does not go to HEOS.
    fn heos_command(
        &self,
        msg: &Command,
    ) -> Option<ResponseActFuture<Self, Result<(), ServiceError>>> {
        let link = self.heos.clone()?;
        let avr = self.avr.as_ref()?;
        let action = match msg.kind {
            EntityKind::MediaPlayer => {
                media_player::command(&msg.cmd_id, msg.params.as_ref()).ok()?
            }
            EntityKind::Remote => {
                let run = remote::command(&msg.cmd_id, msg.params.as_ref()).ok()?;
                if run.steps() != 1 {
                    return None;
                }
                run.actions.into_iter().next()?
            }
            _ => return None,
        };
        if !commands::is_heos_action(&action)
            || !commands::heos_in_use(&link, &avr.state().borrow())
        {
            return None;
        }
        let deadline = msg.deadline;
        let name = self.cfg.name.clone();
        let cmd_name = command_name(msg).to_owned();
        Some(Box::pin(
            async move {
                let result = commands::heos_run(&link, &action, Some(deadline)).await;
                sent(result, &name, &cmd_name, false)
            }
            .into_actor(self),
        ))
    }

    /// Send the configuration to the controller, which saves it. The version lets the
    /// controller drop a configuration that is older than its own.
    fn send_config(&self) {
        self.controller.do_send(DeviceEvent::Config {
            key: self.key.clone(),
            version: self.cfg_version,
            cfg: Box::new(self.cfg.clone()),
        });
    }

    /// Store the form of the tuner presets when the receiver shows a new one. The controller saves
    /// it. A new connection does not know the form until the receiver reports a preset.
    fn save_preset_form(&mut self) {
        let Some(avr) = &self.avr else { return };
        let lettered = avr.state().borrow().device().tuner.presets_lettered;
        if lettered.is_none() || lettered == self.cfg.tuner_presets_lettered {
            return;
        }
        self.cfg.tuner_presets_lettered = lettered;
        self.send_config();
    }

    /// Ask the names of the tuner presets or network favorites once per telnet connection, when the
    /// main zone is on and plays the tuner or network audio. The names are not asked while they are
    /// not used.
    fn ask_names(&mut self) {
        let Some(avr) = self.avr.clone().filter(|_| self.telnet_up) else {
            return;
        };
        let reads = discovery::ExtraReads::new(&self.cfg);
        let Some(names) = discovery::Names::in_use(reads, &avr.state().borrow()) else {
            return;
        };
        if self.names_asked & names.bit() != 0 {
            return;
        }
        self.names_asked |= names.bit();
        actix::spawn(async move { names.read(&avr).await });
    }

    /// Save the names when no answer came for `NAMES_SAVE_DELAY`. The answers of all 56 presets
    /// take about 6 seconds.
    fn schedule_names_save(&mut self, names: discovery::Names, ctx: &mut Context<Self>) {
        self.names_pending |= names.bit();
        if let Some(handle) = self.timers.names.take() {
            ctx.cancel_future(handle);
        }
        self.timers.names = Some(ctx.run_later(NAMES_SAVE_DELAY, |act, _| {
            act.timers.names = None;
            act.save_names();
        }));
    }

    /// Store the new names that are pending. The controller saves them. The select shows the
    /// stored names.
    fn save_names(&mut self) {
        let pending = std::mem::take(&mut self.names_pending);
        let Some(avr) = &self.avr else { return };
        let mut changed = false;
        for names in [
            discovery::Names::TunerPresets,
            discovery::Names::NetworkFavorites,
        ] {
            if pending & names.bit() == 0 {
                continue;
            }
            let Some(merged) = names.merged(&self.cfg, &avr.state().borrow()) else {
                continue;
            };
            match names {
                discovery::Names::TunerPresets => self.cfg.tuner_preset_names = merged,
                discovery::Names::NetworkFavorites => self.cfg.network_favorite_names = merged,
            }
            changed = true;
        }
        if !changed {
            return;
        }
        self.dirty_labels = true;
        self.dirty_full = true;
        self.flush_emit();
        self.send_config();
    }

    /// Take what the configuration keeps of the receiver: the generation, the HTTP read method,
    /// `CommApiVers`, the tuner, the receiver values for the next connect, and the serial number
    /// and MAC address if they are not stored yet. A configuration of the Python integration does
    /// not have them.
    ///
    /// The receiver kind is not taken if the answers do not prove the type: it can be wrong, and
    /// a new kind removes entities. Returns true if the configuration changed: the caller sends it
    /// to the controller, which saves it.
    fn update_receiver_info(&mut self, info: &Info, version: u64) -> bool {
        if self.cfg_version != version {
            return false;
        }
        let kind = discovery::receiver_kind(info);
        let new_kind = info.type_proof.is_proven() && kind != self.cfg.kind();
        if !info.type_proof.is_proven() {
            debug!(
                "[{}] receiver type not proven, kind not saved",
                self.cfg.name
            );
        }
        let new_ids = self
            .cfg
            .add_ids(info.serial_number.as_deref(), info.mac_address.as_deref());
        // Values with a type that is not proven are not stored: the next connect would keep them.
        let new_info = info.type_proof.is_proven() && self.cfg.info.as_ref() != Some(info);
        if new_info {
            self.cfg.info = Some(info.clone());
            // New values can come with other sources. At a connect, only a look-up connect
            // gives new values, also with a new address: it did not use the missing sources and
            // asks them again. `note_missing` stores the ones that are missing.
            // `info_changed` connects again.
            self.cfg.missing = MissingSources::default();
        }
        if !new_kind && !new_ids && !new_info {
            return false;
        }
        if new_kind {
            info!("[{}] receiver: {kind:?}", self.cfg.name);
            self.cfg.set_kind(kind);
        }
        if new_ids {
            info!(
                "[{}] receiver ids: serial {:?}, MAC {:?}",
                self.cfg.name, self.cfg.serial, self.cfg.mac
            );
        }
        true
    }

    /// Store the HTTP sources that the receiver showed it does not have. Only new ones are added:
    /// a connect without the stored values counts a source as there until a read tried it. The
    /// controller saves them.
    fn note_missing(&mut self) {
        let Some(avr) = &self.avr else { return };
        let missing = crate::config_store::missing_union(self.cfg.missing, avr.missing_sources());
        if missing == self.cfg.missing {
            return;
        }
        info!("[{}] missing over HTTP: {missing:?}", self.cfg.name);
        self.cfg.missing = missing;
        self.send_config();
    }

    /// The receiver gave other values than the stored ones. Another receiver at the address is
    /// not used: the receiver is looked up until it is found. Otherwise the values and the receiver
    /// kind are stored. A new type, port or update method changes how the receiver is read: it is
    /// connected again with them.
    fn info_changed(&mut self, info: Info, version: u64, ctx: &mut Context<Self>) {
        if let Some(e) = other_receiver(&self.cfg, &info) {
            info!("[{}] {e}", self.cfg.name);
            self.look_up = true;
            self.lost_connection(ctx);
            self.connect(ctx);
            return;
        }
        // New values can come with other sources, for example after a firmware update. The
        // connection keeps the missing sources, so connect again without them. With an old
        // configuration version the values are not stored, so the flags stay.
        let clear = info.type_proof.is_proven()
            && version == self.cfg_version
            && self.cfg.missing != MissingSources::default();
        if clear {
            debug!("[{}] missing sources cleared", self.cfg.name);
            self.cfg.missing = MissingSources::default();
        }
        let reconnect = clear
            || self
                .avr
                .as_ref()
                .is_some_and(|avr| read_differently(avr.info(), &info));
        info!(
            "[{}] receiver values changed: {} on port {}, {:?}",
            self.cfg.name,
            info.receiver_type.name(),
            info.port,
            info.update_method
        );
        if self.update_receiver_info(&info, version) {
            self.send_config();
        }
        if reconnect {
            self.lost_connection(ctx);
            self.connect(ctx);
        }
    }

    /// Take the address that was found by the host name or the search, see
    /// `DeviceConfig::set_address`. Returns true if the configuration changed: the caller sends it
    /// to the controller, which saves it.
    fn update_address(&mut self, address: Option<String>, version: u64) -> bool {
        let Some(address) = address else { return false };
        if self.cfg_version != version || address == self.cfg.address {
            return false;
        }
        info!(
            "[{}] address changed: {} -> {address}",
            self.cfg.name, self.cfg.address
        );
        let had_host = self.cfg.host.is_some();
        self.cfg.set_address(address);
        if !had_host && let Some(host) = &self.cfg.host {
            info!("[{}] host name kept: {host}", self.cfg.name);
        }
        // The times of the old address do not apply.
        self.connect_times = discovery::ConnectTimes::default();
        true
    }

    /// Read the state over HTTP and show if the receiver answers. It does nothing while telnet
    /// reports the changes, or while an earlier read runs. The signal has its own timer, see
    /// `poll_signal`.
    fn poll_http(&mut self, ctx: &mut Context<Self>) {
        if !self.needs_poll() || self.polling {
            return;
        }
        let Some(avr) = self.avr.clone() else { return };
        self.polling = true;
        let generation = self.generation;
        let handle = ctx.spawn(async move { avr.update().await }.into_actor(self).map(
            move |result, act, ctx| {
                // A result of an older connection must not clear the flag of the new one.
                if act.generation != generation {
                    return;
                }
                act.polling = false;
                act.timers.poll_read = None;
                act.note_missing();
                let ok = result.is_ok();
                if let Err(e) = &result {
                    debug!("[{}] read over HTTP failed: {e}", act.cfg.name);
                }
                // Changes of the values come as events. The entities show the link too.
                let flipped = act.http_ok != ok;
                act.http_ok = ok;
                if ok {
                    act.poll_failures = 0;
                    act.http_checked = true;
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
            },
        ));
        self.timers.poll_read = Some(handle);
    }

    /// The time of the signal timer, see [`signal_every`].
    fn signal_every(&self) -> Option<Duration> {
        let every = signal_every(&self.cfg, self.poll_interval)?;
        Some(self.signal_interval.unwrap_or(every))
    }

    /// Read the signal over HTTP: Audyssey, the signal and the HDMI output
    /// (`update_signal_and_settings`, only on receivers that use `AppCommand`) and the signal
    /// pages. HDR, pixel depth, color space and FRL come only from the video page. With a signal
    /// time of 0, only `update_signal_and_settings`. `POLL_READS` in `config_store` names these
    /// reads: keep them the same.
    ///
    /// In telnet mode it reads only while telnet is down: telnet reports the signal, and on
    /// receivers with `CommApiVers` 0301 or higher the library checks it
    /// (`TelnetConfig::signal_check`). While telnet is down, a blank answer clears values that
    /// telnet gave, for example the Audyssey values of a receiver with Dirac Live; telnet gives
    /// them again after a reconnect. It reads only while the state read works; in telnet mode,
    /// only after a state read that worked since telnet went down. It runs on its own, so a web
    /// interface that does not answer does not delay the state. One read runs at a time.
    fn poll_signal(&mut self, ctx: &mut Context<Self>) {
        // A slow receiver must not get a new read before the old one is done.
        if self.signal_polling
            || !self.http_ok
            || (self.cfg.use_telnet && (self.telnet_up || !self.http_checked))
        {
            return;
        }
        let Some(avr) = self.avr.clone() else { return };
        self.signal_polling = true;
        let generation = self.generation;
        let pages = self.cfg.signal_secs() > 0;
        let handle = ctx.spawn(
            async move {
                if pages {
                    avr.refresh_signal_pages().await
                } else {
                    avr.update_signal_and_settings().await.map(|_| ())
                }
            }
            .into_actor(self)
            .map(move |result, act, _| {
                // `NothingRead`: no page gave values, for example all answered 404.
                if let Err(e) = result
                    && !matches!(e, AvrError::NothingRead)
                {
                    debug!("[{}] signal not read: {e}", act.cfg.name);
                }
                if act.generation == generation {
                    act.signal_polling = false;
                    act.timers.signal_read = None;
                    act.note_missing();
                }
            }),
        );
        self.timers.signal_read = Some(handle);
    }

    /// Drop the connection and mark the entities unavailable.
    fn lost_connection(&mut self, ctx: &mut Context<Self>) {
        self.close(ctx);
        self.set_status(DeviceStatus::Disconnected);
        self.keep_state = false;
        self.emit();
    }

    /// Close the connection, cancel the timers and reads of it and stop the start task of the
    /// library. Commands, telnet asks and the other tasks of the library end on their own.
    fn close(&mut self, ctx: &mut Context<Self>) {
        // Names that came before the close are saved now: the next connection may not ask again.
        if let Some(handle) = self.timers.names.take() {
            ctx.cancel_future(handle);
            self.save_names();
        }
        self.generation += 1;
        self.attempt.send_modify(|a| *a += 1);
        self.stale_config = false;
        // A connection that drops at once keeps the reconnect delay growing.
        if self
            .connected_at
            .take()
            .is_some_and(|t| t.elapsed() >= STABLE_CONNECTION)
        {
            self.attempts = 0;
        }
        // The `Avr` owns the HEOS connection.
        self.drop_heos(ctx);
        self.telnet_up = false;
        self.http_ok = false;
        self.http_checked = false;
        self.polling = false;
        self.signal_polling = false;
        self.refreshing = false;
        // An older connect attempt must not run on: it would read the receiver in parallel with
        // the next one.
        self.cancel_connect(ctx);
        if let Some(stopper) = self.start_stopper.take() {
            stopper.stop();
        }
        for handle in [
            self.timers.poll.take(),
            self.timers.poll_read.take(),
            self.timers.signal.take(),
            self.timers.signal_read.take(),
            self.timers.refresh.take(),
            self.timers.pages.take(),
            self.timers.telnet.take(),
        ]
        .into_iter()
        .flatten()
        {
            ctx.cancel_future(handle);
        }
        self.early_tx.send_replace(None);
        if let Some(avr) = self.avr.take() {
            // Keep the last state for the entities.
            self.last = avr.snapshot();
            actix::spawn(async move { avr.telnet_disconnect().await });
        }
    }

    /// Cancel the timers and reads of the receiver connection. It does not reset the flags of the
    /// reads: call `close` first.
    fn cancel_timers(&mut self, ctx: &mut Context<Self>) {
        let t = std::mem::take(&mut self.timers);
        for handle in [
            t.connect,
            t.reconnect,
            t.poll,
            t.poll_read,
            t.signal,
            t.signal_read,
            t.refresh,
            t.pages,
            t.telnet,
            t.heos,
            t.names,
        ]
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

    /// A new main-zone source, a change of the source list, a main-zone power change, or a title
    /// that plays again ends the pause.
    fn clear_pause(&mut self, change: Change) {
        if !self.paused {
            return;
        }
        let playing = self.avr.as_ref().is_some_and(|avr| {
            avr.state().borrow().device().now_playing.is_playing() == Some(true)
        });
        if ends_pause(change.setting, change.zone, playing) {
            self.paused = false;
        }
    }

    /// Whether network audio plays now.
    fn net_audio_playing(&self) -> bool {
        self.avr
            .as_ref()
            .is_some_and(|avr| avr.state().borrow().device().now_playing.is_playing() == Some(true))
    }

    /// Start a remote command with more than one step. It is answered
    /// at once, as the Python integration does: the Remote waits at most 10 s for an answer. The
    /// steps do not hold up the other commands. `None` for a command with one step: it runs as
    /// the other commands.
    fn remote_run(
        &self,
        msg: &Command,
        connecting: &mut Option<Connecting>,
        ctx: &mut Context<Self>,
    ) -> Option<Result<(), ServiceError>> {
        match remote::command(&msg.cmd_id, msg.params.as_ref()) {
            Err(e) => Some(Err(e)),
            Ok(run) if run.steps() > 1 => {
                self.spawn_run(run, msg.clone(), connecting.take(), ctx);
                Some(Ok(()))
            }
            Ok(_) => None,
        }
    }

    /// Run the steps of a remote command in the background. It first waits for a running connect,
    /// until the deadline of `msg`. A stop ends the wait and the run.
    fn spawn_run(
        &self,
        run: remote::RemoteRun,
        msg: Command,
        connecting: Option<Connecting>,
        ctx: &mut Context<Self>,
    ) {
        let lock = self.cmd_lock.clone();
        let cfg = self.cfg.clone();
        let expected = self.expected.clone();
        let received = Instant::now();
        let mut stop = self.run_stop.subscribe();
        let disconnects = self.disconnects;
        let attempt = connecting.as_ref().map(|c| c.attempt.clone());
        let deadline = msg.deadline;
        ctx.spawn(
            async move {
                // A stop ends the wait for the connect.
                let stopped = tokio::select! {
                    biased;
                    _ = stop.changed() => true,
                    _ = timeout_at(deadline, connect_ended(connecting)) => false,
                };
                (stop, stopped)
            }
            .into_actor(self)
            .then(move |(stop, stopped), act, _| {
                let avr = act.avr.clone();
                let heos = act.heos.clone();
                let name = act.cfg.name.clone();
                let stale = act.stale(attempt.as_ref(), disconnects);
                async move {
                    if stopped || stop.has_changed().unwrap_or(true) {
                        debug!("[{name}] {} stopped, steps sent: 0", command_name(&msg));
                        return;
                    }
                    if stale {
                        log_stale(&name, &msg);
                        return;
                    }
                    let Some(avr) = avr else {
                        warn!(
                            "[{name}] {}: the receiver is not connected",
                            command_name(&msg)
                        );
                        return;
                    };
                    log_sending(&name, &msg, received, false);
                    let result = commands::run_steps(
                        &avr,
                        heos.as_ref(),
                        &cfg,
                        &expected,
                        &run,
                        &lock,
                        stop,
                    )
                    .await;
                    if let Ok(Some(steps)) = result {
                        debug!(
                            "[{name}] {} stopped, steps sent: {steps}",
                            command_name(&msg)
                        );
                    }
                    let _ = sent(result.map(drop), &name, command_name(&msg), false);
                }
                .into_actor(act)
            }),
        );
    }

    /// Remember the pause after a media command. The receiver sends its own changes: only the
    /// media player is sent, and only if the pause changed.
    fn track_pause(&mut self, cmd: &str, was_playing: bool, ctx: &mut Context<Self>) {
        let paused = match cmd {
            "play_pause" => !self.paused && was_playing,
            "stop" | "next" | "previous" | "off" | "toggle" => false,
            _ => self.paused,
        };
        if paused != self.paused {
            self.paused = paused;
            self.dirty_part |= PART_PLAYER;
            self.arm_emit(ctx);
        }
    }

    /// Where a command goes now. Before the start ends, only a command that does not read the
    /// state (`without_state`) gets the receiver.
    fn target(&self, without_state: bool) -> Target {
        let early = self.avr.is_none() && without_state;
        let avr = if early {
            self.early_tx.borrow().clone()
        } else {
            self.avr.clone()
        };
        Target {
            avr,
            heos: self.heos.clone(),
            // Before the start ends, the network of the Remote can still be down after wake-up.
            // The library tries the command again then, but not after standby or a new wake-up.
            attempt: early.then(|| self.attempt.subscribe()),
        }
    }

    /// Start a connect if none runs. During the wait of a [`Disconnect`], see
    /// [`Self::want_again`].
    fn start_connect(&mut self, ctx: &mut Context<Self>) {
        if !self.want && self.avr.is_some() {
            self.want_again(ctx);
            return;
        }
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

    /// Start a connect for a command if there is no connection. While a [`Disconnect`] waits for
    /// a running command, the connection is kept.
    fn connect_for_command(&mut self, ctx: &mut Context<Self>) {
        // The core can send a command before `exit_standby`. While a retry after a failed connect
        // is pending, the receiver is not reachable: the command fails at once.
        let backoff = self.want && self.timers.reconnect.is_some();
        if self.avr.is_none() && self.status == DeviceStatus::Disconnected && !backoff {
            debug!("[{}] command while disconnected, connecting", self.cfg.name);
            self.start_connect(ctx);
        } else if !self.want && self.avr.is_some() {
            debug!(
                "[{}] command during disconnect, connection kept",
                self.cfg.name
            );
            self.want_again(ctx);
        }
    }

    /// Keep the connection during the wait of a [`Disconnect`]. After a setup change in the wait,
    /// connects again with the new values.
    fn want_again(&mut self, ctx: &mut Context<Self>) {
        self.want = true;
        if self.stale_config {
            self.reconnect_for_config(ctx);
        }
    }

    /// Connect again after a setup change.
    fn reconnect_for_config(&mut self, ctx: &mut Context<Self>) {
        info!("[{}] configuration changed, reconnecting", self.cfg.name);
        // A command from before the change is not sent again on the new connection.
        self.disconnects += 1;
        self.lost_connection(ctx);
        self.attempts = 0;
        self.connect(ctx);
    }
}

/// Find the receiver and start it. Retries network errors fast, which covers the short Wi-Fi
/// drop of the Remote after wake-up. `Err(None)` means the receiver is not on the network.
///
/// With `known`, the stored values and address are used and nothing is looked up. The start
/// checks them, also that it is the same receiver. If that start fails, the receiver is looked up
/// at once.
///
/// The tries of the identify and the start have a time limit from `last`, the fastest of the last
/// starts, see [`discovery::Stall`]. The search on the network has none.
///
/// `early` gets the receiver before its start: commands that do not read the state can use it.
async fn connect_avr(
    cfg: DeviceConfig,
    known: Option<Info>,
    last: Option<Duration>,
    early: impl Fn(Arc<Avr>),
) -> Result<Connected, Option<AvrError>> {
    let deadline = Instant::now() + discovery::NETWORK_RETRY_TIMEOUT;
    let mut stall = discovery::Stall::new(last);
    if let Some(info) = known {
        let avr = discovery::connect_known(&cfg.address, info, cfg.missing, cfg.signal_secs())
            .await
            .map_err(Some)?;
        match start_avr(avr, None, cfg.use_telnet, deadline, &mut stall, &early).await {
            Ok(connected) => return Ok(connected),
            // The address or the values can be old.
            Err(e) => debug!("[{}] connect with the stored values failed: {e}", cfg.name),
        }
    }
    let (avr, address) = look_up(&cfg, deadline, &mut stall).await?;
    start_avr(avr, address, cfg.use_telnet, deadline, &mut stall, &early)
        .await
        .map_err(Some)
}

/// Start the receiver and subscribe to its changes. `early` gets the receiver before the start.
async fn start_avr(
    avr: Avr,
    address: Option<String>,
    telnet: bool,
    deadline: Instant,
    stall: &mut discovery::Stall,
    early: impl Fn(Arc<Avr>),
) -> Result<Connected, AvrError> {
    let avr = Arc::new(avr);
    // Subscribe before the start, so that no change is missed.
    let changes = avr.changes();
    early(avr.clone());
    let (started, took) = discovery::start_retry(&avr, telnet, deadline, stall).await?;
    Ok(Connected {
        avr,
        changes,
        address,
        started,
        took,
    })
}

/// Identify the receiver at the stored address, else at the addresses of the host name, else search
/// for it. Also gives the address if it is not the one of the configuration.
///
/// Another receiver at the stored address is not used: the receiver is then searched by its MAC
/// address or serial number.
async fn look_up(
    cfg: &DeviceConfig,
    deadline: Instant,
    stall: &mut discovery::Stall,
) -> Result<(Avr, Option<String>), Option<AvrError>> {
    let mut address = None;
    let mut found = None;
    let mut last_error = None;
    let known = Some(discovery::Known::of(cfg));
    // The last good address first.
    if !cfg.address.is_empty() {
        match discovery::identify_retry(&cfg.address, known, deadline, stall).await {
            Ok(avr) => match other_receiver(cfg, avr.info()) {
                Some(e) => {
                    info!("[{}] {e}", cfg.name);
                    last_error = Some(e);
                }
                None => found = Some(avr),
            },
            Err(e) => {
                debug!("[{}] last address failed: {e}", cfg.name);
                last_error = Some(e);
            }
        }
    }
    // The host name can point to a new address.
    if found.is_none()
        && let Some(host) = cfg.host.as_deref().filter(|h| *h != cfg.address)
        && let Some((avr, ip)) =
            identify_by_name(cfg, host, known, deadline, stall, &mut last_error).await
    {
        address = Some(ip);
        found = Some(avr);
    }
    // The address can have changed: search for the MAC address or serial number.
    if found.is_none() {
        let receivers = discovery::discover_for(cfg).await.map_err(Some)?;
        let Some(receiver) = discovery::find(cfg, &receivers) else {
            return Err(last_error);
        };
        let avr = discovery::identify_retry(&receiver.host, known, deadline, stall)
            .await
            .map_err(Some)?;
        if let Some(e) = other_receiver(cfg, avr.info()) {
            return Err(Some(e));
        }
        address = Some(receiver.host.clone());
        found = Some(avr);
    }
    Ok((found.ok_or(last_error)?, address))
}

/// Resolve the host name and identify the receiver at its addresses, but not at the stored one.
/// Gives the receiver and its address. The checks are the ones of the stored address. A failed
/// identify goes to `last_error`.
///
/// The stored address can take all the time until `deadline`: the name then gets
/// [`discovery::NETWORK_RETRY_TIMEOUT`] from now for the resolve and the identify.
async fn identify_by_name(
    cfg: &DeviceConfig,
    host: &str,
    known: Option<discovery::Known>,
    deadline: Instant,
    stall: &mut discovery::Stall,
    last_error: &mut Option<AvrError>,
) -> Option<(Avr, String)> {
    let deadline = deadline.max(Instant::now() + discovery::NETWORK_RETRY_TIMEOUT);
    let ips = match timeout_at(deadline.into(), discovery::resolve(host)).await {
        Ok(Ok(ips)) => ips,
        Ok(Err(e)) => {
            info!("[{}] host name {host} not resolved: {e}", cfg.name);
            return None;
        }
        Err(_) => {
            info!("[{}] host name {host} not resolved in time", cfg.name);
            return None;
        }
    };
    info!("[{}] host name {host} resolved to {ips:?}", cfg.name);
    for ip in ips.into_iter().filter(|ip| *ip != cfg.address) {
        match discovery::identify_retry(&ip, known, deadline, stall).await {
            Ok(avr) => match other_receiver(cfg, avr.info()) {
                Some(e) => {
                    info!("[{}] {e}", cfg.name);
                    *last_error = Some(e);
                }
                None => return Some((avr, ip)),
            },
            Err(e) => {
                debug!("[{}] address {ip} of {host} failed: {e}", cfg.name);
                *last_error = Some(e);
            }
        }
    }
    None
}

/// The new values change how the receiver is read: the type, the port or the update method.
pub(super) fn read_differently(old: &Info, new: &Info) -> bool {
    old.receiver_type != new.receiver_type
        || old.port != new.port
        || old.update_method != new.update_method
}

/// An error if the receiver is not the one of the configuration. A receiver that gives neither id
/// is used.
fn other_receiver(cfg: &DeviceConfig, info: &Info) -> Option<AvrError> {
    let same = cfg.same_receiver(info.serial_number.as_deref(), info.mac_address.as_deref());
    (same == Some(false)).then(|| {
        AvrError::InvalidRequest(format!(
            "another receiver answers at {}: serial {:?}, MAC {:?}",
            info.host, info.serial_number, info.mac_address
        ))
    })
}

/// Wait until a command can be sent, until [`SEND_TIME`] before `deadline`. Nothing is sent while
/// it waits: it can answer with a timeout and be tried again.
///
/// The commands keep their order: one that waits for the connect holds the lock, so the ones after
/// it wait too. One that does not read the state waits only for the receiver.
async fn wait_turn(
    lock: Arc<tokio::sync::Mutex<()>>,
    connecting: Option<Connecting>,
    early: watch::Receiver<Option<Arc<Avr>>>,
    without_state: bool,
    deadline: tokio::time::Instant,
) -> Result<tokio::sync::OwnedMutexGuard<()>, ServiceError> {
    let Some(end) = deadline
        .checked_sub(SEND_TIME)
        .filter(|end| tokio::time::Instant::now() < *end)
    else {
        return Err(ServiceError::Timeout);
    };
    timeout_at(end, async {
        let guard = lock.lock_owned().await;
        if without_state {
            connect_usable(connecting, early).await;
        } else {
            connect_ended(connecting).await;
        }
        guard
    })
    .await
    .map_err(|_| ServiceError::Timeout)
}

/// Answer with the result of `send`, or with a timeout at `deadline`. A send that is not done then
/// is not stopped: it ends in the background, so that a telnet line is not cut. A lock that `send`
/// holds stays held until the send ends, so the next commands still come after it. `finish` gets
/// the output of the send, and `true` when the answer was a timeout.
async fn answer_by<T>(
    send: impl Future<Output = T> + 'static,
    deadline: tokio::time::Instant,
    finish: impl FnOnce(T, bool) -> Result<(), ServiceError> + 'static,
) -> Result<(), ServiceError> {
    let mut send = Box::pin(send);
    tokio::select! {
        biased;
        result = &mut send => finish(result, false),
        () = sleep_until(deadline) => {
            actix::spawn(async move {
                let _ = finish(send.await, true);
            });
            Err(ServiceError::Timeout)
        }
    }
}

/// Wait until a running connect has a receiver that takes commands or has ended, or until the
/// connection is closed or another connect starts. The caller limits the wait.
async fn connect_usable(
    connecting: Option<Connecting>,
    mut early: watch::Receiver<Option<Arc<Avr>>>,
) {
    let Some(Connecting {
        mut status,
        mut attempt,
    }) = connecting
    else {
        return;
    };
    tokio::select! {
        _ = status.wait_for(|s| *s != DeviceStatus::Connecting) => {}
        _ = attempt.changed() => {}
        _ = early.wait_for(Option::is_some) => {}
    }
}

/// Wait until a running connect has ended, or until the connection is closed or another connect
/// starts. The caller limits the wait.
async fn connect_ended(connecting: Option<Connecting>) {
    if let Some(Connecting {
        mut status,
        mut attempt,
    }) = connecting
    {
        tokio::select! {
            _ = status.wait_for(|s| *s != DeviceStatus::Connecting) => {}
            _ = attempt.changed() => {}
        }
    }
}

impl Actor for DenonActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.me = Some(ctx.address().downgrade());
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

/// The selects with many options: their options change only with the main-zone power or new
/// stored names.
const fn has_many_options(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Select(SelectKind::NetworkFavorite | SelectKind::TunerPreset)
    )
}

/// The attributes of the entities that `keep` takes.
fn build_changes(
    view: &View<'_>,
    keep: impl Fn(EntityKind) -> bool,
    labels: bool,
) -> Vec<(EntityKind, Map<String, serde_json::Value>)> {
    EntityKind::all(view.cfg)
        .filter(|kind| keep(*kind))
        .map(|kind| {
            let mut attrs = Map::new();
            if labels || !has_many_options(kind) {
                entities::attributes(view, kind, &mut attrs);
            } else {
                entities::current_attributes(view, kind, &mut attrs);
            }
            (kind, attrs)
        })
        .collect()
}

/// Whether a change is of the main zone or of the whole receiver.
const fn is_main(zone: Option<denon_marantz::Zone>) -> bool {
    matches!(zone, None | Some(denon_marantz::Zone::Main))
}

/// A new main-zone source, a change of the source list or a main-zone power change ends a pause.
/// So does a title that plays again. A new source name can change what the source plays.
const fn ends_pause(setting: Setting, zone: Option<denon_marantz::Zone>, playing: bool) -> bool {
    match setting {
        Setting::Source | Setting::Power => is_main(zone),
        Setting::Sources => true,
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
/// the reasons to read values again. Only a main-zone source changes the digital codec.
const fn reload_after(setting: Setting, zone: Option<denon_marantz::Zone>) -> Option<u8> {
    match setting {
        Setting::SpeakerPreset => Some(RELOAD_PRESET),
        Setting::DiracFilter => Some(RELOAD_FILTER),
        Setting::DynamicEq => Some(RELOAD_DYNAMIC_EQ),
        Setting::MultiEq => Some(RELOAD_MULTI_EQ),
        Setting::Source if is_main(zone) => Some(RELOAD_SOURCE),
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
            if let Some(parts) = reload_after(change.setting, change.zone) {
                self.schedule_reload(ctx, parts);
            }
            self.clear_pause(change);
            // The kept sound mode can be missing from a new list.
            if change.setting == Setting::SoundModes {
                self.sound_mode = None;
            }
            match change.setting {
                Setting::Tuner => {
                    self.save_preset_form();
                    self.schedule_names_save(discovery::Names::TunerPresets, ctx);
                }
                Setting::NetworkPresetNames => {
                    self.schedule_names_save(discovery::Names::NetworkFavorites, ctx);
                }
                Setting::Source | Setting::Power if is_main(change.zone) => {
                    // The library sends a change only when the value changed, or for the
                    // source, when its kind changed.
                    if let Some(avr) = &self.avr {
                        let main = denon_marantz::Zone::Main;
                        if change.setting == Setting::Power {
                            debug!(
                                "[{}] power: {:?}",
                                self.cfg.name,
                                avr.state().borrow().zone(main).power
                            );
                        } else {
                            debug!(
                                "[{}] source: {:?}",
                                self.cfg.name,
                                avr.state().borrow().zone(main).source
                            );
                        }
                    }
                    self.ask_names();
                }
                // A new name can change what the main source plays.
                Setting::Sources => self.ask_names(),
                _ => {}
            }
            self.schedule_emit_for(change, ctx);
        } else {
            // Changes were lost.
            self.schedule_emit(ctx);
        }
    }
}

impl Handler<StartSynced> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: StartSynced, ctx: &mut Self::Context) {
        if msg.0 != self.generation {
            return;
        }
        // A receiver without a list has sent none until now: the groups are the choices.
        self.sound_groups = true;
        self.note_missing();
        self.ask_names();
        self.schedule_emit(ctx);
    }
}

impl Handler<Early> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: Early, _: &mut Self::Context) {
        if msg.0 == *self.attempt.borrow() && self.want && self.status == DeviceStatus::Connecting {
            self.early_tx.send_replace(Some(msg.1));
        }
    }
}

impl Handler<SendAgain> for DenonActor {
    type Result = Option<Again>;

    fn handle(&mut self, msg: SendAgain, _: &mut Self::Context) -> Self::Result {
        if !self.want || msg.disconnects != self.disconnects {
            return None;
        }
        let target = self.target(msg.without_state);
        let connecting = target
            .avr
            .is_none()
            .then(|| self.running_connect())
            .flatten();
        let early = (connecting.is_some() && msg.without_state).then(|| self.early_tx.subscribe());
        Some(Again {
            target,
            connecting,
            early,
        })
    }
}

impl Handler<InfoChanged> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: InfoChanged, ctx: &mut Self::Context) {
        let InfoChanged(generation, version, info) = msg;
        if generation == self.generation {
            self.info_changed(info, version, ctx);
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
        if msg.1 {
            self.names_asked = 0;
            self.ask_names();
        }
        // After a reconnect, the library asks for its own groups again. The driver reads what only
        // HTTP gives and its extra values: asking for all groups again would repeat the queries.
        if msg.1
            && !self.refreshing
            && let Some(avr) = self.avr.clone()
        {
            self.refreshing = true;
            let generation = self.generation;
            let handle = ctx.spawn(
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
                        act.timers.refresh = None;
                    }
                }),
            );
            self.timers.refresh = Some(handle);
        }
        // While telnet is down, the state is read over HTTP. The signal waits for a read that
        // works: `http_ok` can be old.
        if !msg.1 {
            self.http_checked = false;
            self.poll_http(ctx);
        }
        self.schedule_emit(ctx);
    }
}

impl Handler<HeosChanged> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: HeosChanged, ctx: &mut Self::Context) {
        let Some(link) = self.heos.as_ref().filter(|_| msg.0 == self.heos_generation) else {
            return;
        };
        if msg
            .1
            .as_ref()
            .is_some_and(|change| !heos::concerns(link.pid(), change))
        {
            return;
        }
        // The library sends a play state change when the value changed, and after the library lost
        // changes. Changes lost here come as `None` and are not logged.
        if matches!(msg.1, Some(HeosChange::PlayState(_))) {
            debug!(
                "[{}] HEOS play state: {:?}",
                self.cfg.name,
                link.with_player(|player| player.play_state).flatten()
            );
        }
        // A position that the Remote counts on by itself is not sent.
        let progress = matches!(msg.1, Some(HeosChange::Progress(_)));
        if !self.update_position() && progress {
            return;
        }
        self.dirty_part |= PART_PLAYER;
        self.arm_emit(ctx);
    }
}

impl Handler<HeosStatusChanged> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: HeosStatusChanged, ctx: &mut Self::Context) {
        if msg.0 != self.heos_generation {
            return;
        }
        let name = &self.cfg.name;
        let link = match msg.1 {
            HeosPlayerStatus::Ready(_) => {
                let link = self
                    .avr
                    .as_ref()
                    .and_then(|avr| avr.heos())
                    .and_then(|player| heos::Link::new(&player));
                if link.is_some() {
                    info!("[{name}] HEOS connected");
                } else if self.heos.is_some() {
                    debug!("[{name}] HEOS player lost: {:?}", msg.1);
                }
                link
            }
            status => {
                if matches!(status, HeosPlayerStatus::WaitingForPlayer) {
                    info!("[{name}] HEOS has no player with this address yet");
                }
                if self.heos.is_some() {
                    debug!("[{name}] HEOS player lost: {status:?}");
                }
                None
            }
        };
        self.set_heos(link, ctx);
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
        self.disconnects += 1;
        // Not connected and not asked to connect: nothing to end. The entities keep their shown
        // state.
        if !self.want && self.avr.is_none() && self.status == DeviceStatus::Disconnected {
            return;
        }
        self.want = false;
        // The other timers stop in `close`: a connect that is asked for during the wait below
        // keeps the connection and needs them.
        self.cancel_connect(ctx);
        if let Some(handle) = self.timers.reconnect.take() {
            ctx.cancel_future(handle);
        }
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
        let disconnects = self.disconnects;
        ctx.spawn(
            async move { timeout(DISCONNECT_WAIT, lock.lock_owned()).await.ok() }
                .into_actor(self)
                .map(move |guard, act, ctx| {
                    drop(guard);
                    // A connect or command that came meanwhile keeps the connection. A later
                    // `Disconnect` closes the connection itself.
                    if act.want {
                        // Like after a connect: a lost link makes the entities unavailable.
                        act.keep_state = false;
                        if !act.linked() {
                            act.schedule_emit(ctx);
                        }
                    } else if act.disconnects == disconnects {
                        act.close(ctx);
                        act.set_status(DeviceStatus::Disconnected);
                    }
                }),
        );
    }
}

#[cfg(test)]
impl Handler<CurrentAvr> for DenonActor {
    type Result = Option<std::sync::Weak<Avr>>;

    fn handle(&mut self, _: CurrentAvr, _: &mut Self::Context) -> Self::Result {
        self.avr.as_ref().map(Arc::downgrade)
    }
}

#[cfg(test)]
impl Handler<CommandLockFree> for DenonActor {
    type Result = bool;

    fn handle(&mut self, _: CommandLockFree, _: &mut Self::Context) -> bool {
        self.cmd_lock.try_lock().is_ok()
    }
}

#[cfg(test)]
impl Handler<HoldCommandLock> for DenonActor {
    type Result = ();

    fn handle(&mut self, msg: HoldCommandLock, _: &mut Self::Context) {
        let guard = self
            .cmd_lock
            .clone()
            .try_lock_owned()
            .expect("free command lock");
        actix::spawn(async move {
            tokio::time::sleep(msg.0).await;
            drop(guard);
        });
    }
}

#[cfg(test)]
impl Handler<Refreshing> for DenonActor {
    type Result = bool;

    fn handle(&mut self, _: Refreshing, _: &mut Self::Context) -> Self::Result {
        self.timers.refresh.is_some()
    }
}

#[cfg(test)]
impl Handler<Wants> for DenonActor {
    type Result = MessageResult<Wants>;

    fn handle(&mut self, _: Wants, _: &mut Self::Context) -> Self::Result {
        MessageResult(WantState {
            want: self.want,
            reconnect: self.timers.reconnect.is_some(),
            status: self.status,
            keep_state: self.keep_state,
        })
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
        let mut cfg = msg.0;
        // A setup copies the names and the form of the tuner presets from the controller. They
        // come from this actor, and the controller can still have older ones.
        if !self.cfg.tuner_preset_names.is_empty() {
            cfg.tuner_preset_names = std::mem::take(&mut self.cfg.tuner_preset_names);
        }
        if !self.cfg.network_favorite_names.is_empty() {
            cfg.network_favorite_names = std::mem::take(&mut self.cfg.network_favorite_names);
        }
        if self.cfg.tuner_presets_lettered.is_some() {
            cfg.tuner_presets_lettered = self.cfg.tuner_presets_lettered;
        }
        let new_address = cfg.address != self.cfg.address;
        // A new setup clears the missing sources: the connection still has them.
        let reconnect = new_address
            || cfg.use_telnet != self.cfg.use_telnet
            || cfg.signal_secs() != self.cfg.signal_secs()
            || (cfg.missing == MissingSources::default()
                && self.cfg.missing != MissingSources::default());
        if new_address {
            self.connect_times = discovery::ConnectTimes::default();
        }
        self.set_config(cfg);
        self.cfg_version += 1;
        if reconnect && self.want {
            self.reconnect_for_config(ctx);
        } else if reconnect && self.avr.is_some() {
            // During the wait of a `Disconnect`. The running command is sent first, unless a
            // connect or command comes in the wait.
            self.stale_config = true;
        }
    }
}

/// The name of a command for the log: the command of a remote entity, else the command id.
fn command_name(msg: &Command) -> &str {
    msg.params
        .as_ref()
        .filter(|_| msg.kind == EntityKind::Remote)
        .and_then(|p| p.get("command"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&msg.cmd_id)
}

/// Run a command. With `attempt`, the command stops once another connect attempt starts or the
/// connection is closed. The library tries again while the connection cannot be built, but it does
/// not know about standby. A stopped command answers OK: the user cannot do anything about it.
pub(super) async fn send_command(
    avr: &Avr,
    heos: Option<&heos::Link>,
    cfg: &DeviceConfig,
    expected: &Expected,
    msg: &Command,
    attempt: Option<watch::Receiver<u64>>,
    name: &str,
) -> Result<(), ServiceError> {
    let send = commands::run(avr, heos, cfg, expected, msg);
    let started = Instant::now();
    let result = if let Some(mut attempt) = attempt {
        if attempt.has_changed().unwrap_or(true) {
            info!(
                "[{name}] {} not sent: standby or a new wake-up",
                command_name(msg)
            );
            return Ok(());
        }
        tokio::select! {
            result = send => result,
            // An error means that the actor is gone.
            _ = attempt.changed() => {
                info!(
                    "[{name}] {} stopped by standby or a new wake-up, it may have run",
                    command_name(msg)
                );
                return Ok(());
            }
        }
    } else {
        send.await
    };
    if result.is_ok() {
        let took = started.elapsed().as_millis();
        debug!("[{name}] {} done in {took} ms", command_name(msg));
    }
    result
}

/// What [`send_again`] needs.
pub(super) struct Resend {
    pub(super) addr: WeakAddr<DenonActor>,
    /// See [`commands::without_state`]. False when the receiver was connected.
    pub(super) without_state: bool,
    /// The value of `DenonActor::disconnects` when the command came.
    pub(super) disconnects: u64,
    /// When the command came.
    pub(super) received: Instant,
}

impl Resend {
    const fn ask(&self) -> SendAgain {
        SendAgain {
            without_state: self.without_state,
            disconnects: self.disconnects,
        }
    }
}

/// Send a command to `target`, and with [`send_again`] once more if it was surely not sent.
async fn send_to(
    target: Target,
    resend: &Resend,
    cfg: &DeviceConfig,
    expected: &Expected,
    msg: &Command,
    name: &str,
) -> Result<(), ServiceError> {
    let first = match &target.avr {
        Some(avr) => {
            let heos = target.heos.as_ref();
            send_command(avr, heos, cfg, expected, msg, target.attempt, name).await
        }
        None => Err(ServiceError::NotConnected),
    };
    match first {
        // Boxed: the state of a resend is not kept in the future of every command.
        Err(ServiceError::NotConnected) => {
            Box::pin(send_again(resend, cfg, expected, msg, name)).await
        }
        other => other,
    }
}

/// Send once more a command that was surely not sent: there was no receiver, or the library
/// answered `NotConnected`. The caller holds the command lock. It waits for a running connect,
/// until [`SEND_TIME`] before the deadline of `msg`. Nothing is sent after a [`Disconnect`] or a
/// reconnect after a setup change since the command came. When nothing is sent, the answer is
/// `Timeout` if the time was over or the connect still runs after the wait, else `NotConnected`.
/// A source command sends power on and then the source: power on can be sent twice.
pub(super) async fn send_again(
    resend: &Resend,
    cfg: &DeviceConfig,
    expected: &Expected,
    msg: &Command,
    name: &str,
) -> Result<(), ServiceError> {
    let cmd = command_name(msg);
    let not_sent = |reason: &str| {
        info!("[{name}] {cmd} not sent: {reason}");
        Err(ServiceError::NotConnected)
    };
    let no_time = || {
        log_no_time(name, msg, resend.received);
        Err(ServiceError::Timeout)
    };
    let Some(end) = msg
        .deadline
        .checked_sub(SEND_TIME)
        .filter(|end| tokio::time::Instant::now() < *end)
    else {
        return no_time();
    };
    let Some(addr) = resend.addr.upgrade() else {
        return not_sent("stopped");
    };
    // An error means that the actor is gone.
    let Ok(Some(again)) = addr.send(resend.ask()).await else {
        return not_sent("disconnected, setup changed or stopped");
    };
    let mut target = again.target;
    let mut still_connecting = false;
    if target.avr.is_none() {
        let Some(connecting) = again.connecting else {
            return not_sent("no connect runs");
        };
        let wait = async {
            match again.early {
                Some(early) => connect_usable(Some(connecting), early).await,
                None => connect_ended(Some(connecting)).await,
            }
        };
        let _ = timeout_at(end, wait).await;
        let Ok(Some(again)) = addr.send(resend.ask()).await else {
            return not_sent("disconnected, setup changed or stopped");
        };
        still_connecting = again.connecting.is_some();
        target = again.target;
    }
    let Some(avr) = target.avr else {
        return if still_connecting {
            no_time()
        } else {
            not_sent("no receiver")
        };
    };
    if tokio::time::Instant::now() >= end {
        return no_time();
    }
    // The send can end after the answer: it does not keep the actor address.
    drop(addr);
    let waited = resend.received.elapsed().as_millis();
    info!("[{name}] {cmd} not sent, sending again after {waited} ms");
    send_command(
        &avr,
        target.heos.as_ref(),
        cfg,
        expected,
        msg,
        target.attempt,
        name,
    )
    .await
}

/// The answer to a sent command, and its log. `late`: the command was answered with a timeout
/// before the send ended.
fn sent(
    result: Result<(), ServiceError>,
    name: &str,
    cmd_name: &str,
    late: bool,
) -> Result<(), ServiceError> {
    let after = if late { " after the answer" } else { "" };
    match result {
        // The state of the receiver does not allow it. The Remote shows an error banner for any
        // other answer than OK that the user must close, and the Python integration did nothing
        // here: the answer is OK, and the log says why nothing happened.
        Err(ServiceError::Blocked(reason)) => {
            info!("[{name}] {cmd_name} ignored{after}: {reason}");
            Ok(())
        }
        Err(e) => {
            warn!("[{name}] {cmd_name} failed{after}: {e}");
            Err(e)
        }
        Ok(()) => {
            if late {
                info!("[{name}] {cmd_name} done after the answer");
            }
            Ok(())
        }
    }
}

/// Log a command that is not sent because its time is over.
fn log_no_time(name: &str, msg: &Command, received: Instant) {
    let waited = received.elapsed().as_millis();
    info!(
        "[{name}] {} not sent: no time left after {waited} ms",
        command_name(msg)
    );
}

/// Whether a command that came during a connect must not be sent: since it came, a [`Disconnect`]
/// or a reconnect after a setup change came (`then` differs from `now`), and the connection was
/// closed or opened again. `attempt` is from [`Connecting`], `None` for a command that came while
/// connected. Such a command, and one whose connection did not change, is sent before a
/// disconnect closes the connection, if the disconnect gets the command lock in time, see the
/// `Disconnect` handler.
fn stale(attempt: Option<&watch::Receiver<u64>>, then: u64, now: u64) -> bool {
    then != now && attempt.is_some_and(|a| a.has_changed().unwrap_or(true))
}

/// Log a command that waited for a connect and is not sent, see [`stale`].
fn log_stale(name: &str, msg: &Command) {
    info!(
        "[{name}] {} not sent: disconnected or setup changed during the wait",
        command_name(msg)
    );
}

/// Log how long a command waited before it goes to the receiver.
fn log_sending(name: &str, msg: &Command, received: Instant, early: bool) {
    let waited = received.elapsed().as_millis();
    let when = if early { " before the start ended" } else { "" };
    debug!(
        "[{name}] sending {} after {waited} ms{when}",
        command_name(msg)
    );
}

impl Handler<Command> for DenonActor {
    type Result = ResponseActFuture<Self, Result<(), ServiceError>>;

    fn handle(&mut self, msg: Command, ctx: &mut Self::Context) -> Self::Result {
        if let Some(answer) = self.answer_at_once(&msg) {
            return Box::pin(fut::ready(answer));
        }
        self.connect_for_command(ctx);
        // A command can come right after the Remote wakes up, while the connect still runs.
        let mut connecting = self.running_connect();
        if self.avr.is_none() && connecting.is_none() {
            return Box::pin(fut::ready(Err(ServiceError::NotConnected)));
        }
        if msg.kind == EntityKind::Remote
            && let Some(answer) = self.remote_run(&msg, &mut connecting, ctx)
        {
            return Box::pin(fut::ready(answer));
        }
        if let Some(run) = self.heos_command(&msg) {
            return run;
        }
        let lock = self.cmd_lock.clone();
        let cfg = self.cfg.clone();
        let expected = self.expected.clone();
        let received = Instant::now();
        let disconnects = self.disconnects;
        let attempt = connecting.as_ref().map(|c| c.attempt.clone());
        // Only a command during a connect can run before the start ends.
        let without_state = self.avr.is_none()
            && commands::without_state(&cfg, msg.kind, &msg.cmd_id, msg.params.as_ref());
        let early = self.early_tx.subscribe();
        // Network audio plays now: a pause command pauses it.
        let was_playing = self.net_audio_playing();
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
        let deadline = msg.deadline;
        Box::pin(
            async move { wait_turn(lock, connecting, early, without_state, deadline).await }
                .into_actor(self)
                .then(move |guard, act, ctx| {
                    let target = act.target(without_state);
                    let name = act.cfg.name.clone();
                    let addr = act.me.clone().unwrap_or_else(|| ctx.address().downgrade());
                    let stale = act.stale(attempt.as_ref(), disconnects);
                    async move {
                        let guard = match guard {
                            Ok(guard) => guard,
                            Err(ServiceError::Timeout) => {
                                log_no_time(&name, &msg, received);
                                return Err(ServiceError::Timeout);
                            }
                            Err(e) => return Err(e),
                        };
                        if stale {
                            log_stale(&name, &msg);
                            return Err(ServiceError::NotConnected);
                        }
                        // The wait can end late when the actor is busy.
                        if tokio::time::Instant::now() >= deadline {
                            log_no_time(&name, &msg, received);
                            return Err(ServiceError::Timeout);
                        }
                        if target.avr.is_some() {
                            log_sending(&name, &msg, received, target.attempt.is_some());
                        }
                        let resend = Resend {
                            addr,
                            without_state,
                            disconnects,
                            received,
                        };
                        let send = async move {
                            let _guard = guard;
                            let result =
                                send_to(target, &resend, &cfg, &expected, &msg, &name).await;
                            (result, name, msg)
                        };
                        let finish = |(result, name, msg): (_, String, Command), late| {
                            sent(result, &name, command_name(&msg), late)
                        };
                        answer_by(send, deadline, finish).await
                    }
                    .into_actor(act)
                })
                .map(move |result, act, ctx| {
                    // The receiver does not tell a pause from a stop: the pause is remembered here, as
                    // the Python integration does.
                    if result.is_ok()
                        && let Some(cmd) = media_command.as_deref()
                    {
                        act.track_pause(cmd, was_playing, ctx);
                    }
                    result
                }),
        )
    }
}

/// The time of the signal timer, see `DeviceConfig::signal_secs`, rounded up to a multiple of the
/// HTTP poll time (`poll`), at least one. It reads over HTTP, so the reads of the state and of the
/// signal stay apart. In telnet mode, 0 turns it off.
fn signal_every(cfg: &DeviceConfig, poll: Duration) -> Option<Duration> {
    let secs = cfg.signal_secs();
    if cfg.use_telnet && secs == 0 {
        return None;
    }
    let poll_ms = u64::try_from(poll.as_millis()).unwrap_or(u64::MAX).max(1);
    let steps = (u64::from(secs) * 1000).div_ceil(poll_ms).max(1);
    Some(Duration::from_millis(steps.saturating_mul(poll_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// A deadline that leaves `wait` for the wait before the send.
    fn deadline_after(wait: Duration) -> tokio::time::Instant {
        tokio::time::Instant::now() + SEND_TIME + wait
    }

    fn no_receiver() -> watch::Receiver<Option<Arc<Avr>>> {
        watch::channel(None).1
    }

    #[actix::test]
    async fn the_wait_for_the_lock_ends_before_the_deadline() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let _held = lock.clone().lock_owned().await;
        let start = Instant::now();
        let deadline = deadline_after(Duration::from_millis(200));
        let result = wait_turn(lock, None, no_receiver(), false, deadline).await;
        assert!(matches!(result, Err(ServiceError::Timeout)));
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(200), "{waited:?}");
        assert!(waited < Duration::from_secs(1), "{waited:?}");
    }

    #[test]
    fn a_command_from_a_connect_is_stale_only_after_a_disconnect_that_closed() {
        let (attempt_tx, attempt) = watch::channel(0);
        // A disconnect that waits for the lock has not closed the connection yet.
        assert!(!stale(Some(&attempt), 0, 1));
        // A command that came while connected.
        assert!(!stale(None, 0, 1));
        attempt_tx.send_modify(|a| *a += 1);
        assert!(stale(Some(&attempt), 0, 1));
        // A reconnect that is not a disconnect or a setup change.
        assert!(!stale(Some(&attempt), 1, 1));
    }

    #[actix::test]
    async fn the_wait_for_a_connect_ends_when_another_connect_starts() {
        let (_status, status) = watch::channel(DeviceStatus::Connecting);
        let (attempt_tx, attempt) = watch::channel(0);
        let (_early_tx, early) = watch::channel(None);
        let connecting = || {
            Some(Connecting {
                status: status.clone(),
                attempt: attempt.clone(),
            })
        };
        let (usable, ended) = (
            connect_usable(connecting(), early),
            connect_ended(connecting()),
        );
        attempt_tx.send_modify(|a| *a += 1);
        let both = async { tokio::join!(usable, ended) };
        timeout(Duration::from_millis(500), both)
            .await
            .expect("the waits end");
    }

    #[actix::test]
    async fn the_wait_for_a_connect_ends_before_the_deadline() {
        let (_status, status) = watch::channel(DeviceStatus::Connecting);
        let (_attempt, attempt) = watch::channel(0);
        for without_state in [false, true] {
            let start = Instant::now();
            let deadline = deadline_after(Duration::from_millis(200));
            let lock = Arc::default();
            let connecting = Some(Connecting {
                status: status.clone(),
                attempt: attempt.clone(),
            });
            // The actor keeps the sender while it runs.
            let (_early_tx, early) = watch::channel(None);
            let result = wait_turn(lock, connecting, early, without_state, deadline).await;
            assert!(matches!(result, Err(ServiceError::Timeout)));
            // After the wait. The connect does not end.
            let waited = start.elapsed();
            assert!(waited >= Duration::from_millis(200), "{waited:?}");
            assert!(waited < Duration::from_secs(1), "{waited:?}");
        }
    }

    #[actix::test]
    async fn a_command_without_time_left_for_the_send_does_not_wait() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let _held = lock.clone().lock_owned().await;
        let start = Instant::now();
        let deadline = tokio::time::Instant::now() + SEND_TIME / 2;
        let result = wait_turn(lock, None, no_receiver(), false, deadline).await;
        assert!(matches!(result, Err(ServiceError::Timeout)));
        let waited = start.elapsed();
        assert!(waited < SEND_TIME / 4, "{waited:?}");
    }

    #[actix::test]
    async fn a_long_send_is_answered_at_the_deadline_and_ends_later() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = lock.clone().lock_owned().await;
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let send_done = done.clone();
        let send = async move {
            let _guard = guard;
            tokio::time::sleep(Duration::from_millis(500)).await;
            send_done.store(true, Ordering::SeqCst);
            Ok(())
        };
        let start = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        let (late_tx, late_rx) = tokio::sync::oneshot::channel();
        let finish = move |result, late| {
            let _ = late_tx.send(late);
            result
        };
        assert_eq!(
            Err(ServiceError::Timeout),
            answer_by(send, deadline, finish).await
        );
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        assert!(waited < Duration::from_millis(400), "{waited:?}");
        // The send goes on and keeps the lock: the next command waits for it.
        assert!(lock.try_lock().is_err());
        let _next = timeout(Duration::from_secs(2), lock.lock())
            .await
            .expect("the send ends");
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(Ok(true), late_rx.await);
    }

    #[actix::test]
    async fn a_send_before_the_deadline_gives_its_result() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let failed = async { Err(ServiceError::NotConnected) };
        assert_eq!(
            Err(ServiceError::NotConnected),
            answer_by(failed, deadline, |result, late| {
                assert!(!late);
                result
            })
            .await
        );
    }

    #[test]
    fn the_signal_timer_is_a_multiple_of_the_http_poll() {
        let every = |use_telnet, secs| {
            let cfg: DeviceConfig =
                serde_json::from_str(r#"{"id":"A","name":"N","address":"h"}"#).expect("json");
            let cfg = DeviceConfig {
                use_telnet,
                signal_poll_secs: secs,
                ..cfg
            };
            signal_every(&cfg, HTTP_POLL_INTERVAL)
        };
        let secs = |s| Some(Duration::from_secs(s));
        assert_eq!(None, every(true, Some(0)));
        assert_eq!(secs(10), every(true, None));
        assert_eq!(secs(30), every(true, Some(30)));
        // A multiple of the poll time.
        assert_eq!(secs(20), every(true, Some(15)));
        assert_eq!(secs(20), every(false, Some(11)));
        // In HTTP mode 0 still reads `AppCommand0300`.
        assert_eq!(secs(10), every(false, Some(0)));
        assert_eq!(secs(10), every(false, Some(5)));
        assert_eq!(secs(30), every(false, Some(30)));
    }

    #[test]
    fn the_log_names_the_remote_command_else_the_command_id() {
        let cmd = |kind, cmd_id: &str, command: Option<&str>| {
            Command::new(
                kind,
                cmd_id.to_owned(),
                command.map(|c| Map::from_iter([("command".to_owned(), c.into())])),
            )
        };
        let remote = cmd(EntityKind::Remote, "send_cmd", Some("MUTE_TOGGLE"));
        assert_eq!("MUTE_TOGGLE", command_name(&remote));
        let sequence = cmd(EntityKind::Remote, "send_cmd_sequence", None);
        assert_eq!("send_cmd_sequence", command_name(&sequence));
        let media = cmd(EntityKind::MediaPlayer, "volume", Some("x"));
        assert_eq!("volume", command_name(&media));
    }

    #[test]
    fn the_position_is_sent_only_when_it_jumps_or_the_play_state_changes() {
        let at = |position, playing, ago: u64| HeosPosition {
            shown: media_player::Position {
                position,
                duration: 200,
            },
            playing,
            at: Instant::now()
                .checked_sub(Duration::from_secs(ago))
                .expect("time"),
        };
        // Played 5 s since 10: 15 is expected, the Remote counts by itself.
        assert!(!position_moved(
            Some(&at(10, true, 5)),
            Some(&at(15, true, 0))
        ));
        assert!(!position_moved(
            Some(&at(10, true, 5)),
            Some(&at(16, true, 0))
        ));
        // A seek.
        assert!(position_moved(
            Some(&at(10, true, 5)),
            Some(&at(60, true, 0))
        ));
        // Paused: the position stays, the state changes.
        assert!(position_moved(
            Some(&at(10, true, 5)),
            Some(&at(15, false, 0))
        ));
        assert!(!position_moved(
            Some(&at(15, false, 5)),
            Some(&at(15, false, 0))
        ));
        // A new item with another length.
        let mut other = at(0, true, 0);
        other.shown.duration = 100;
        assert!(position_moved(Some(&at(10, true, 5)), Some(&other)));
        assert!(position_moved(None, Some(&other)));
        assert!(position_moved(Some(&other), None));
        assert!(!position_moved(None, None));
    }

    fn test_cfg() -> DeviceConfig {
        DeviceConfig {
            id: "SERIAL0000001".into(),
            id_kind: crate::config_store::IdKind::Serial,
            serial: None,
            mac: None,
            name: "AVR".into(),
            address: "192.0.2.1".into(),
            host: None,
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
            signal_poll_secs: Some(0),
            receiver_type: None,
            update_method: None,
            comm_api_version: None,
            has_tuner: None,
            tuner_presets_lettered: None,
            tuner: None,
            tuner_preset_names: Vec::new(),
            network_favorite_names: Vec::new(),
            info: None,
            missing: MissingSources::default(),
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
            let mode = select::active_sound_mode(state, true);
            let view = View {
                state,
                cfg: &cfg,
                connected: true,
                artwork_url: None,
                paused: false,
                sound_mode: mode,
                sound_groups: true,
                heos: None,
                position: None,
            };
            build_changes(&view, |_| true, true)
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
        assert_eq!(
            Some(RELOAD_PRESET),
            reload_after(Setting::SpeakerPreset, None)
        );
        let (_, queries) = RELOAD_QUERIES[0];
        assert!(queries.contains(&Query::DiracSlots));
        assert!(queries.contains(&Query::DiracFilter));
    }

    #[test]
    fn a_filter_change_does_not_read_the_filter() {
        assert_eq!(
            Some(RELOAD_FILTER),
            reload_after(Setting::DiracFilter, None)
        );
        assert!(!FILTER_VALUES.contains(&Query::DiracFilter));
    }

    #[test]
    fn an_audyssey_change_reads_the_values_that_depend_on_it() {
        assert_eq!(
            Some(RELOAD_DYNAMIC_EQ),
            reload_after(Setting::DynamicEq, None)
        );
        assert_eq!(Some(RELOAD_MULTI_EQ), reload_after(Setting::MultiEq, None));
    }

    #[test]
    fn a_new_main_zone_source_reads_the_digital_codec() {
        let main = Some(denon_marantz::Zone::Main);
        assert_eq!(Some(RELOAD_SOURCE), reload_after(Setting::Source, main));
        for zone in [denon_marantz::Zone::Zone2, denon_marantz::Zone::Zone3] {
            assert_eq!(None, reload_after(Setting::Source, Some(zone)));
        }
    }

    #[test]
    fn every_reason_has_queries() {
        for (bit, queries) in RELOAD_QUERIES {
            assert_eq!(1, bit.count_ones());
            assert_ne!(0, queries.len());
        }
    }

    #[test]
    fn a_new_source_a_source_list_change_or_a_power_change_end_the_pause() {
        let main = Some(denon_marantz::Zone::Main);
        assert!(ends_pause(Setting::Source, main, false));
        assert!(ends_pause(Setting::Source, None, false));
        assert!(ends_pause(Setting::Sources, None, false));
        assert!(ends_pause(Setting::Power, main, false));
    }

    #[test]
    fn a_change_in_another_zone_does_not_end_the_pause() {
        for zone in [denon_marantz::Zone::Zone2, denon_marantz::Zone::Zone3] {
            assert!(!ends_pause(Setting::Source, Some(zone), false));
            assert!(!ends_pause(Setting::Power, Some(zone), false));
        }
    }

    #[test]
    fn a_title_that_plays_again_ends_the_pause() {
        assert!(ends_pause(Setting::NowPlaying, None, true));
        assert!(!ends_pause(Setting::NowPlaying, None, false));
        assert!(!ends_pause(Setting::Volume, None, true));
    }

    #[test]
    fn other_settings_are_not_read_again() {
        for setting in [Setting::Volume, Setting::Dimmer, Setting::NowPlaying] {
            assert_eq!(None, reload_after(setting, None), "{setting:?}");
        }
    }
}
