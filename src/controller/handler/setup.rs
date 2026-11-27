// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Driver setup flow: discovery, options and reconfiguration.

use crate::config_store::{
    DEFAULT_SIGNAL_SECS, DeviceConfig, DriverSettings, Feature, MIN_SIGNAL_SECS, SaveWait,
    backup_json, is_ip, parse_backup, same_ids,
};
use crate::controller::devices::{ConnectTrigger, sorted_configs};
use crate::controller::handler::AbortDriverSetup;
use crate::controller::{Controller, operation_mode};
use crate::device::discovery;
use crate::errors::ServiceError;
use crate::logging;
use actix::{ActorFutureExt, AsyncContext, Context, Handler, Message, WrapFuture};
use denon_marantz::{Avr, Error as AvrError, TunerInfo};
use log::{debug, info, warn};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use uc_api::intg::{DriverSetupChange, IntegrationSetup, SetupDriver};
use uc_api::model::intg::{IntegrationSetupError, IntegrationSetupState, SetupChangeEventType};
use uc_api::ws::{EventCategory, WsMessage};

/// Time between the acknowledgment of a setup request and the next setup event, for the web
/// configurator. The order does not depend on it: the acknowledgment goes first through the same
/// queue.
const ACK_DELAY: Duration = Duration::from_millis(100);
/// Highest time between signal reads in seconds.
const MAX_SIGNAL_POLL_SECS: u16 = 3600;
/// Longest time for the resolve of one entered host name and the identify of all its IP
/// addresses. The resolve takes at most 3 s, and an address that cannot be reached fails after
/// about 3 s of connect tries: this leaves time for two such addresses.
const ENTERED_TIMEOUT: Duration = Duration::from_secs(10);

/// Start the setup flow.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetupDriverMsg {
    pub ws_id: String,
    pub data: SetupDriver,
    /// Request id and response message. The handler sends the acknowledgment. `None`: no
    /// acknowledgment.
    pub ack: Option<(u32, &'static str)>,
}

/// User input of a setup page.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetDriverUserDataMsg {
    pub ws_id: String,
    pub data: IntegrationSetup,
    /// Request id and response message. The handler sends the acknowledgment. `None`: no
    /// acknowledgment.
    pub ack: Option<(u32, &'static str)>,
}

/// Receiver found in the setup flow.
#[derive(Debug, Clone)]
pub struct Found {
    /// Address of the receiver. Identifies the receiver on the setup pages.
    address: String,
    /// The host name that the user entered.
    host: Option<String>,
    name: String,
    model: String,
    serial: Option<String>,
    /// 12 uppercase hexadecimal digits.
    mac: Option<String>,
    is_denon: bool,
    is_dirac_supported: bool,
    /// `None` if the setup could not find it out.
    has_tuner: Option<bool>,
    /// See `DeviceConfig::has_signal_check`. True if the setup could not find it out.
    signal_check: bool,
}

impl Found {
    fn new(receiver: &denon_marantz::DiscoveredReceiver, detected: Detected) -> Self {
        Self {
            address: receiver.host.clone(),
            host: None,
            name: receiver
                .friendly_name
                .clone()
                .or_else(|| receiver.model_name.clone())
                .unwrap_or_else(|| receiver.host.clone()),
            model: receiver.model_name.clone().unwrap_or_default(),
            serial: receiver.serial_number.clone(),
            mac: receiver
                .mac_address
                .as_deref()
                .and_then(crate::config_store::mac_text),
            is_denon: receiver.brand != denon_marantz::Brand::Marantz,
            is_dirac_supported: detected.dirac,
            has_tuner: detected.tuner,
            signal_check: detected.signal_check,
        }
    }

    /// A receiver that the user gave the address of.
    fn from_identified(
        entered: Entered,
        id: &discovery::Identified,
        has_tuner: Option<bool>,
    ) -> Self {
        Self {
            address: entered.address,
            host: entered.host,
            name: id.name.clone(),
            model: id.model.clone(),
            serial: id.serial.clone(),
            mac: id.mac.clone(),
            is_denon: id.is_denon,
            is_dirac_supported: id.is_dirac_supported,
            has_tuner,
            signal_check: signal_check(&id.info),
        }
    }
}

/// Options of a receiver in the setup pages.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
struct Options {
    show_all_inputs: bool,
    is_denon: bool,
    use_telnet: bool,
    is_dirac_supported: bool,
    has_tuner: bool,
    /// The setup could not find out if the receiver has a tuner: the field asks the user.
    tuner_unknown: bool,
    signal_poll_secs: u16,
    /// Show the signal field. Without the check of the library it applies only to the reads over
    /// HTTP (HTTP mode, and telnet mode while telnet is down), which are in steps of 10 s.
    signal_check: bool,
}

impl Options {
    fn of(cfg: &DeviceConfig) -> Self {
        Self {
            show_all_inputs: cfg.show_all_inputs,
            is_denon: cfg.is_denon,
            use_telnet: cfg.use_telnet,
            is_dirac_supported: cfg.has_dirac(),
            has_tuner: cfg.offers(Feature::Tuner),
            tuner_unknown: cfg.tuner.is_none() && cfg.has_tuner.is_none(),
            // A value from a backup or a changed file can be above the field limit.
            signal_poll_secs: cfg.signal_secs().min(MAX_SIGNAL_POLL_SECS),
            signal_check: cfg.has_signal_check(),
        }
    }

    const fn for_found(found: &Found) -> Self {
        Self {
            show_all_inputs: false,
            is_denon: found.is_denon,
            use_telnet: true,
            is_dirac_supported: found.is_dirac_supported,
            has_tuner: !matches!(found.has_tuner, Some(false)),
            tuner_unknown: found.has_tuner.is_none(),
            signal_poll_secs: DEFAULT_SIGNAL_SECS,
            signal_check: found.signal_check,
        }
    }

    /// Read the option fields of a page. A missing field keeps the value of `self`.
    fn read(mut self, values: &HashMap<String, String>) -> Result<Self, String> {
        self.show_all_inputs = parse_bool(values, "show_all_inputs", self.show_all_inputs);
        if let Some(m) = values.get("manufacturer") {
            self.is_denon = m.to_ascii_lowercase().starts_with("denon");
        }
        if let Some(mode) = values.get("connection_mode") {
            self.use_telnet = mode == "use_telnet";
        }
        if let Some(v) = values.get("is_dirac_supported") {
            self.is_dirac_supported = v.eq_ignore_ascii_case("true");
        }
        if let Some(v) = values.get("has_tuner") {
            self.has_tuner = v.eq_ignore_ascii_case("true");
        }
        if let Some(secs) = values.get("signal_poll_secs") {
            let secs = secs.trim();
            self.signal_poll_secs = if secs.is_empty() {
                DEFAULT_SIGNAL_SECS
            } else {
                secs.parse()
                    .ok()
                    .filter(|s| {
                        *s == 0 || (MIN_SIGNAL_SECS..=MAX_SIGNAL_POLL_SECS).contains(s)
                    })
                    .ok_or_else(|| {
                        format!(
                            "The signal check time must be 0 or from {MIN_SIGNAL_SECS} to {MAX_SIGNAL_POLL_SECS}"
                        )
                    })?
            };
        }
        Ok(self)
    }

    /// The fields of the page, with the labels of the Python integration.
    fn fields(self) -> Vec<Value> {
        let mut fields = vec![
            json!({
                "id": "show_all_inputs",
                "label": { "en": "Show all sources" },
                "field": { "checkbox": { "value": self.show_all_inputs } }
            }),
            json!({
                "id": "manufacturer",
                "label": { "en": "Select manufacturer" },
                "field": { "dropdown": {
                    "value": if self.is_denon { "denon" } else { "marantz" },
                    "items": [
                        { "id": "denon", "label": { "en": "Denon" } },
                        { "id": "marantz", "label": { "en": "Marantz" } }
                    ]
                } }
            }),
            json!({
                "id": "connection_mode",
                "label": { "en": "Connection mode" },
                "field": { "dropdown": {
                    "value": if self.use_telnet { "use_telnet" } else { "use_http" },
                    "items": [
                        { "id": "use_telnet", "label": { "en": "Use Telnet connection" } },
                        { "id": "use_http", "label": { "en": "Use HTTP connection" } }
                    ]
                } }
            }),
            json!({
                "id": "is_dirac_supported",
                "label": { "en": "Device supports Dirac" },
                "field": { "dropdown": {
                    "value": if self.is_dirac_supported { "True" } else { "False" },
                    "items": [
                        { "id": "True", "label": { "en": "Yes" } },
                        { "id": "False", "label": { "en": "No" } }
                    ]
                } }
            }),
            json!({
                "id": "has_tuner",
                "label": { "en": if self.tuner_unknown {
                    "Receiver has an AM/FM tuner (not found out: turn the receiver on to find it out, or choose)"
                } else {
                    "Receiver has an AM/FM tuner"
                } },
                "field": { "dropdown": {
                    "value": if self.has_tuner { "True" } else { "False" },
                    "items": [
                        { "id": "True", "label": { "en": "Yes" } },
                        { "id": "False", "label": { "en": "No" } }
                    ]
                } }
            }),
            json!({
                "id": "info",
                "label": { "en": "Please note:" },
                "field": { "label": { "value": { "en":
                    "Using telnet provides realtime updates for many values but certain receivers allow a single connection only!"
                } } }
            }),
        ];
        // Without the check of the library, the field would apply only to the reads over HTTP.
        if self.signal_check {
            let signal = json!({
                "id": "signal_poll_secs",
                "label": { "en": "Check the signal info every N seconds (0: off, else 3 or more). Over HTTP (without telnet or while it is down): rounded up to steps of 10 s; without telnet 0 reads only the main signal values, not the signal web pages" },
                "field": { "number": {
                    "value": self.signal_poll_secs, "min": 0, "max": MAX_SIGNAL_POLL_SECS,
                    "steps": 1, "decimals": 0, "unit": { "en": "s" }
                } }
            });
            // Before the note.
            fields.insert(fields.len() - 1, signal);
        }
        fields
    }
}

/// Page the setup flow waits for.
pub enum SetupState {
    /// Optional IP address for the discovery.
    Discover,
    /// First setup: restore a backup or continue with the discovery.
    RestoreChoice,
    /// Backup text of the devices. The page is only read by the user.
    Backup,
    /// Backup text to restore.
    Restore,
    /// Reconfigure: device and action.
    ConfigMode,
    /// List of found devices and the options for the new device.
    ChooseDevice { found: Vec<Found> },
    /// Options of a configured device.
    Configure { key: String },
}

/// Error of the input of a setup page.
enum InputError {
    /// Invalid input. The page is shown again with the message.
    Invalid(Box<SetupState>, String),
    /// The setup flow ends with this error.
    Failed(IntegrationSetupError),
}

/// Checked action of the configuration mode page.
enum ConfigAction {
    Settings,
    Add,
    /// Remove the device with this key.
    Remove(String),
    /// Show the options of this device.
    Configure(Box<DeviceConfig>),
    /// Show this backup text.
    Backup(String),
    Restore,
    Reset,
}

/// A change of the configured devices. The setup writes it to `config.json` first, then
/// changes the memory.
enum DeviceChange {
    /// Replace all devices.
    Restore(Vec<DeviceConfig>),
    /// Remove the device with this key.
    Remove(String),
    /// Set the options of the device with this key. `None` keeps the address and the host name.
    Configure {
        key: String,
        address: Option<Entered>,
        options: Options,
    },
    /// Add a receiver. `update`: the receiver was configured when the setup read it.
    /// `replace`: remove all other devices.
    Add {
        cfg: Box<DeviceConfig>,
        update: bool,
        replace: bool,
    },
}

/// An address of a setup page.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entered {
    /// See `DeviceConfig::address`.
    address: String,
    /// See `DeviceConfig::host`.
    host: Option<String>,
}

impl Entered {
    /// An IP address, without a host name.
    const fn ip(address: String) -> Self {
        Self {
            address,
            host: None,
        }
    }

    /// The host name `host` with the first of its IP addresses where the receiver with `ids`
    /// answers, or a receiver that cannot be compared with it. If none answers within `limit`, the
    /// first IP address. If the name cannot be resolved in that time, the name. `limit` includes
    /// the resolve.
    async fn resolve(host: String, ids: ReceiverIds, limit: Duration) -> Self {
        let deadline = Instant::now() + limit;
        let mut ips = Vec::new();
        let answered = tokio::time::timeout_at(deadline, async {
            ips = resolve_name(&host).await;
            for (i, ip) in ips.iter().enumerate() {
                match discovery::identify(ip, None).await {
                    Ok(avr) if ids.same(avr.info()) != Some(false) => return Some(i),
                    Ok(_) => debug!("[{host}] another receiver answers at {ip}"),
                    Err(e) => debug!("[{host}] address {ip} failed: {e}"),
                }
            }
            None
        })
        .await
        .unwrap_or_else(|_| {
            info!("[{host}] no answer within {limit:?}");
            None
        });
        let address = match answered {
            Some(i) => ips.swap_remove(i),
            None if !ips.is_empty() => ips.swap_remove(0),
            None => host.clone(),
        };
        Self {
            address,
            host: Some(host),
        }
    }
}

/// The serial number and the MAC address of a configured receiver, to find it at an address.
struct ReceiverIds {
    serial: Option<String>,
    mac: Option<String>,
}

impl ReceiverIds {
    fn of(cfg: &DeviceConfig) -> Self {
        Self {
            serial: cfg.serial().map(str::to_owned),
            mac: cfg.mac().map(str::to_owned),
        }
    }

    /// Whether the receiver of `info` is this one. See [`same_ids`].
    fn same(&self, info: &denon_marantz::Info) -> Option<bool> {
        same_ids(
            self.serial.as_deref(),
            self.mac.as_deref(),
            info.serial_number.as_deref(),
            info.mac_address.as_deref(),
        )
    }
}

#[cfg(test)]
thread_local! {
    /// The IP addresses of each host name in the tests on this thread. Empty: no name can be
    /// resolved. `None`: the system resolves the names.
    static TEST_RESOLVE: std::cell::RefCell<Option<Vec<String>>> = const {
        std::cell::RefCell::new(None)
    };
}

/// The IP addresses of a host name. Empty if it cannot be resolved within
/// [`discovery::NETWORK_RETRY_TIMEOUT`].
async fn resolve_name(host: &str) -> Vec<String> {
    #[cfg(test)]
    if let Some(ips) = TEST_RESOLVE.with(|slot| slot.borrow().clone()) {
        return ips;
    }
    match tokio::time::timeout(discovery::NETWORK_RETRY_TIMEOUT, discovery::resolve(host)).await {
        Ok(Ok(ips)) => {
            info!("[{host}] host name resolved to {ips:?}");
            ips
        }
        Ok(Err(e)) => {
            info!("[{host}] host name not resolved: {e}");
            Vec::new()
        }
        Err(_) => {
            info!("[{host}] host name not resolved in time");
            Vec::new()
        }
    }
}

/// Set the options of a setup page. `None` keeps the address and the host name.
fn set_options(cfg: &mut DeviceConfig, address: Option<&Entered>, options: Options) {
    cfg.show_all_inputs = options.show_all_inputs;
    cfg.is_denon = options.is_denon;
    cfg.use_telnet = options.use_telnet;
    cfg.is_dirac_supported = Some(options.is_dirac_supported);
    cfg.tuner = Some(options.has_tuner);
    cfg.signal_poll_secs = Some(options.signal_poll_secs);
    if let Some(entered) = address {
        entered.address.clone_into(&mut cfg.address);
        cfg.host.clone_from(&entered.host);
    }
}

/// Copy from the configured device `old` the fields that the setup does not read from the
/// receiver: the id, the fields that the driver does not use, and the names and the form of the
/// tuner presets that the receiver reported. [`take_unused`] takes the same fields.
fn keep_unused(cfg: &mut DeviceConfig, old: &DeviceConfig) {
    keep_unused_values(cfg, old);
    cfg.id.clone_from(&old.id);
    cfg.tuner_preset_names.clone_from(&old.tuner_preset_names);
    cfg.network_favorite_names
        .clone_from(&old.network_favorite_names);
}

/// The same as [`keep_unused`], but moves the id and the names out of `old`.
fn take_unused(cfg: &mut DeviceConfig, old: DeviceConfig) {
    keep_unused_values(cfg, &old);
    cfg.id = old.id;
    cfg.tuner_preset_names = old.tuner_preset_names;
    cfg.network_favorite_names = old.network_favorite_names;
}

/// Copy the fields that [`keep_unused`] and [`take_unused`] copy by value.
const fn keep_unused_values(cfg: &mut DeviceConfig, old: &DeviceConfig) {
    cfg.id_kind = old.id_kind;
    cfg.update_audyssey = old.update_audyssey;
    cfg.zone2 = old.zone2;
    cfg.zone3 = old.zone3;
    cfg.timeout = old.timeout;
    cfg.volume_step = old.volume_step;
    cfg.tuner_presets_lettered = old.tuner_presets_lettered;
}

impl Handler<SetupDriverMsg> for Controller {
    type Result = Result<(), ServiceError>;

    fn handle(&mut self, msg: SetupDriverMsg, ctx: &mut Self::Context) -> Self::Result {
        debug!("[{}] {:?}", msg.ws_id, msg.data);
        if self
            .sm_consume(&msg.ws_id, &operation_mode::Input::SetupDriverRequest, ctx)
            .is_err()
        {
            return Err(ServiceError::BadRequest(
                "Cannot start driver setup. Please abort setup first.".into(),
            ));
        }
        self.send_ack(&msg.ws_id, msg.ack);
        let reconfigure = msg.data.reconfigure == Some(true);
        if let Some(session) = self.sessions.get_mut(msg.ws_id.as_str()) {
            session.reconfiguring = msg.data.reconfigure;
        }
        self.end_setup_flow();
        self.setup_owner = Some(msg.ws_id.as_str().into());
        let flow = self.setup_flow;
        let ws_id = msg.ws_id;
        if reconfigure {
            ctx.run_later(ACK_DELAY, move |act, ctx| {
                if !act.is_setup_flow(flow, &ws_id) {
                    return;
                }
                let page = act.config_mode_page();
                act.setup = Some(SetupState::ConfigMode);
                act.request_input(&ws_id, page, ctx);
            });
        } else {
            // A first setup replaces the configuration when it ends with a receiver.
            self.setup_replace = true;
            ctx.run_later(ACK_DELAY, move |act, ctx| {
                if act.is_setup_flow(flow, &ws_id) {
                    act.setup = Some(SetupState::RestoreChoice);
                    let page = restore_choice_page(act.driver_settings);
                    act.request_input(&ws_id, page, ctx);
                }
            });
        }
        Ok(())
    }
}

impl Handler<SetDriverUserDataMsg> for Controller {
    type Result = Result<(), ServiceError>;

    fn handle(&mut self, msg: SetDriverUserDataMsg, ctx: &mut Self::Context) -> Self::Result {
        debug!("[{}] setup user data", msg.ws_id);
        if self
            .setup_owner
            .as_deref()
            .is_some_and(|owner| owner != msg.ws_id)
        {
            return Err(ServiceError::BadRequest(
                "Setup was started by another Remote.".into(),
            ));
        }
        let IntegrationSetup::InputValues(values) = msg.data else {
            return Err(ServiceError::BadRequest(
                "Invalid response: require input_values".into(),
            ));
        };
        if self
            .sm_consume(&msg.ws_id, &operation_mode::Input::SetupUserData, ctx)
            .is_err()
        {
            return Err(ServiceError::BadRequest(
                "Not waiting for driver user data. Please restart setup.".into(),
            ));
        }
        let ws_id = msg.ws_id;
        self.send_ack(&ws_id, msg.ack);
        let flow = self.setup_flow;
        let Some(step) = self.setup.take() else {
            warn!("[{ws_id}] no active setup step");
            self.finish_setup_later(ws_id, Some(IntegrationSetupError::Other), ctx);
            return Ok(());
        };
        match self.setup_input(&ws_id, step, &values, ctx) {
            Ok(()) => {}
            Err(InputError::Invalid(step, message)) => {
                info!("[{ws_id}] invalid setup input: {message}");
                // Show the same page again.
                self.setup = Some(*step);
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_input_again(&ws_id, &message, ctx);
                    }
                });
            }
            Err(InputError::Failed(error)) => self.finish_setup_later(ws_id, Some(error), ctx),
        }
        Ok(())
    }
}

impl Handler<AbortDriverSetup> for Controller {
    type Result = ();

    fn handle(&mut self, msg: AbortDriverSetup, ctx: &mut Self::Context) {
        self.abort_setup(&msg.ws_id, msg.timeout, ctx);
    }
}

impl Controller {
    /// Acknowledge a setup request. It goes through the event queue, so it comes before the
    /// setup events that follow.
    fn send_ack(&self, ws_id: &str, ack: Option<(u32, &'static str)>) {
        let Some((req_id, msg)) = ack else { return };
        if let Err(e) = self.respond(ws_id, req_id, msg, ()) {
            warn!("[{ws_id}] setup acknowledgment: {e}");
        }
    }

    /// Abort the setup flow, because of a timeout, an abort event or a disconnect of the Remote.
    pub(super) fn abort_setup(&mut self, ws_id: &str, timeout: bool, ctx: &mut Context<Self>) {
        debug!("[{ws_id}] abort driver setup, timeout: {timeout}");
        if self
            .setup_owner
            .as_deref()
            .is_some_and(|owner| owner != ws_id)
        {
            warn!("[{ws_id}] setup was started by another Remote, not aborting");
            return;
        }
        if timeout {
            self.finish_setup(ws_id, Some(IntegrationSetupError::Timeout), ctx);
            return;
        }
        self.end_setup_flow();
        if self
            .sm_consume(ws_id, &operation_mode::Input::AbortSetup, ctx)
            .is_err()
        {
            return;
        }
        // An aborted setup keeps the configured devices.
        if let Some(session) = self.sessions.get_mut(ws_id) {
            session.reconfiguring = None;
        }
        if !self.devices.is_empty() {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
            self.update_connections(ConnectTrigger::Wake);
        }
    }

    /// Forget the active setup flow. Tasks of the old flow are ignored.
    fn end_setup_flow(&mut self) {
        self.setup = None;
        self.setup_owner = None;
        self.setup_replace = false;
        self.setup_flow = self.setup_flow.wrapping_add(1);
    }

    /// Returns `true` if `flow` is the active setup flow.
    fn is_setup_flow(&self, flow: u32, ws_id: &str) -> bool {
        let active = self.setup_flow == flow;
        if !active {
            debug!("[{ws_id}] ignoring result of an old setup flow");
        }
        active
    }

    /// Handle the input of a setup page.
    fn setup_input(
        &mut self,
        ws_id: &str,
        step: SetupState,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let flow = self.setup_flow;
        let ws_id = ws_id.to_string();
        match step {
            SetupState::Discover => {
                let address = parse_address(values.get("address").map(String::as_str))
                    .map_err(|e| InputError::Invalid(Box::new(SetupState::Discover), e))?;
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.discover(ws_id, address, ctx);
                    }
                });
            }
            SetupState::RestoreChoice => self.restore_choice(ws_id, values, ctx),
            SetupState::Backup => self.finish_setup_later(ws_id, None, ctx),
            SetupState::Restore => self.restore(ws_id, values, ctx)?,
            SetupState::ConfigMode => self.config_mode(ws_id, values, ctx)?,
            SetupState::ChooseDevice { mut found } => {
                let choice = values.get("choice").map_or_default(String::as_str);
                let Some(i) = found.iter().position(|f| f.address == choice) else {
                    return Err(InputError::Invalid(
                        Box::new(SetupState::ChooseDevice { found }),
                        "Unknown device".into(),
                    ));
                };
                let options = match Options::for_found(&found[i]).read(values) {
                    Ok(o) => o,
                    Err(e) => {
                        return Err(InputError::Invalid(
                            Box::new(SetupState::ChooseDevice { found }),
                            e,
                        ));
                    }
                };
                let found = found.swap_remove(i);
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.add_found(ws_id, found, options, ctx);
                    }
                });
            }
            SetupState::Configure { key } => {
                let address = match parse_address(values.get("address").map(String::as_str)) {
                    Ok(a) => a,
                    Err(e) => {
                        return Err(InputError::Invalid(
                            Box::new(SetupState::Configure { key }),
                            e,
                        ));
                    }
                };
                let Some(device) = self.devices.get(key.as_str()) else {
                    return Err(InputError::Failed(IntegrationSetupError::NotFound));
                };
                let options = match Options::of(&device.cfg).read(values) {
                    Ok(o) => o,
                    Err(e) => {
                        return Err(InputError::Invalid(
                            Box::new(SetupState::Configure { key }),
                            e,
                        ));
                    }
                };
                // The address that the page showed keeps the address and the host name.
                let address = address.filter(|a| a != device.cfg.shown_address());
                match address {
                    Some(host) if !is_ip(&host) => {
                        let ids = ReceiverIds::of(&device.cfg);
                        let resolve = Entered::resolve(host, ids, ENTERED_TIMEOUT);
                        ctx.spawn(resolve.into_actor(self).map(move |entered, act, ctx| {
                            if act.is_setup_flow(flow, &ws_id) {
                                let change = DeviceChange::Configure {
                                    key,
                                    address: Some(entered),
                                    options,
                                };
                                act.apply_after_save(ws_id, change, ctx);
                            }
                        }));
                    }
                    address => {
                        let change = DeviceChange::Configure {
                            key,
                            address: address.map(Entered::ip),
                            options,
                        };
                        self.apply_after_save(ws_id, change, ctx);
                    }
                }
            }
        }
        Ok(())
    }

    /// First setup: restore a backup or continue with the discovery.
    fn restore_choice(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) {
        let debug_log = parse_bool(values, "debug_log", self.driver_settings.debug_log);
        self.set_debug_log(&ws_id, debug_log);
        let flow = self.setup_flow;
        let restore = parse_bool(values, "restore_from_backup", false);
        ctx.run_later(ACK_DELAY, move |act, ctx| {
            if !act.is_setup_flow(flow, &ws_id) {
                return;
            }
            if restore {
                act.setup = Some(SetupState::Restore);
                act.request_input(&ws_id, restore_page(), ctx);
            } else {
                act.request_discover(&ws_id, ctx);
            }
        });
    }

    /// Replace the devices with the devices of the pasted backup.
    ///
    /// Nothing of the pasted data is logged.
    fn restore(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let devices = values
            .get("restore_data")
            .and_then(|text| parse_backup(text))
            .ok_or_else(|| {
                warn!("[{ws_id}] invalid restore data");
                InputError::Invalid(Box::new(SetupState::Restore), RESTORE_FAILED.into())
            })?;
        info!("[{ws_id}] restoring {} device(s)", devices.len());
        self.apply_after_save(ws_id, DeviceChange::Restore(devices), ctx);
        Ok(())
    }

    /// Ask for an optional IP address, then discover.
    fn request_discover(&mut self, ws_id: &str, ctx: &mut Context<Self>) {
        self.setup = Some(SetupState::Discover);
        self.request_input(ws_id, discover_page(), ctx);
    }

    /// Look for receivers and show the ones that are not configured.
    fn discover(&self, ws_id: String, address: Option<String>, ctx: &mut Context<Self>) {
        info!("[{ws_id}] discovering receivers, address: {address:?}");
        let flow = self.setup_flow;
        ctx.spawn(
            async move { find_receivers(address).await }
                .into_actor(self)
                .map(move |result, act, ctx| {
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    let found = match result {
                        Ok(f) => f,
                        Err(e) => {
                            warn!("[{ws_id}] discovery failed: {e}");
                            act.finish_setup(&ws_id, Some(setup_error(&e)), ctx);
                            return;
                        }
                    };
                    // A setup that replaces the receivers offers the configured ones too.
                    let found: Vec<Found> = found
                        .into_iter()
                        .filter(|f| {
                            act.setup_replace
                                || act
                                    .find_device(f.serial.as_deref(), f.mac.as_deref())
                                    .is_none()
                        })
                        .collect();
                    if found.is_empty() {
                        info!("[{ws_id}] no new receiver found");
                        act.finish_setup(&ws_id, Some(IntegrationSetupError::NotFound), ctx);
                        return;
                    }
                    let page = choose_device_page(&found);
                    act.setup = Some(SetupState::ChooseDevice { found });
                    act.request_input(&ws_id, page, ctx);
                }),
        );
    }

    /// Read the data of the chosen receiver and save it.
    fn add_found(&self, ws_id: String, found: Found, options: Options, ctx: &mut Context<Self>) {
        info!("[{ws_id}] adding {} ({})", found.name, found.model);
        let flow = self.setup_flow;
        let host = found.address.clone();
        ctx.spawn(
            async move { identify_host(&host).await }
                .into_actor(self)
                .map(move |result, act, ctx| {
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    let identified = match result {
                        Ok(i) => i,
                        Err(e) => {
                            warn!("[{ws_id}] reading the receiver failed: {e}");
                            act.finish_setup(&ws_id, Some(e), ctx);
                            return;
                        }
                    };
                    // A receiver that is configured already is updated: it keeps its id, which
                    // can be the serial number of the Python integration.
                    let old =
                        act.find_device(identified.serial.as_deref(), identified.mac.as_deref());
                    let mut cfg = DeviceConfig {
                        update_audyssey: false,
                        zone2: false,
                        zone3: false,
                        timeout: 2000,
                        id: identified.id,
                        id_kind: identified.id_kind,
                        serial: identified.serial,
                        mac: identified.mac,
                        name: identified.name,
                        address: found.address,
                        host: found.host,
                        support_sound_mode: identified.support_sound_mode,
                        show_all_inputs: options.show_all_inputs,
                        use_telnet: options.use_telnet,
                        volume_step: 0.5,
                        is_denon: options.is_denon,
                        is_dirac_supported: Some(identified.is_dirac_supported),
                        signal_poll_secs: Some(options.signal_poll_secs),
                        receiver_type: identified.kind.receiver_type,
                        update_method: identified.kind.update_method,
                        comm_api_version: identified.kind.comm_api_version,
                        has_tuner: identified.kind.has_tuner,
                        tuner: Some(options.has_tuner),
                        tuner_presets_lettered: None,
                        tuner_preset_names: Vec::new(),
                        network_favorite_names: Vec::new(),
                        info: Some(identified.info),
                        // A new setup asks all sources again.
                        missing: denon_marantz::MissingSources::default(),
                    };
                    let update = old.is_some();
                    if let Some(old) = old {
                        keep_unused(&mut cfg, old);
                    }
                    let change = DeviceChange::Add {
                        cfg: Box::new(cfg),
                        update,
                        replace: act.setup_replace,
                    };
                    act.apply_after_save(ws_id, change, ctx);
                }),
        );
    }

    /// Check the action of the configuration mode page and the chosen device.
    fn check_config_action(&self, action: &str, key: &str) -> Result<ConfigAction, InputError> {
        let invalid =
            |message: String| InputError::Invalid(Box::new(SetupState::ConfigMode), message);
        let unknown_device = || invalid("Unknown device".into());
        Ok(match action {
            "settings" => ConfigAction::Settings,
            "add" => ConfigAction::Add,
            "remove" if self.devices.contains_key(key) => ConfigAction::Remove(key.to_owned()),
            "remove" => return Err(unknown_device()),
            "configure" => ConfigAction::Configure(Box::new(
                self.devices
                    .get(key)
                    .ok_or_else(unknown_device)?
                    .cfg
                    .clone(),
            )),
            "backup" => ConfigAction::Backup(
                backup_json(&sorted_configs(&self.devices))
                    .filter(|_| !self.devices.is_empty())
                    .ok_or_else(|| invalid("Nothing to back up".into()))?,
            ),
            "restore" => ConfigAction::Restore,
            "reset" => ConfigAction::Reset,
            _ => return Err(invalid(format!("Unknown action: {action}"))),
        })
    }

    /// Handle the configuration mode page. The debug log setting is saved only for valid input.
    fn config_mode(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let flow = self.setup_flow;
        let action = self.check_config_action(
            values.get("action").map_or_default(String::as_str),
            values.get("choice").map_or_default(String::as_str),
        )?;
        self.set_debug_log(
            &ws_id,
            parse_bool(values, "debug_log", self.driver_settings.debug_log),
        );
        match action {
            ConfigAction::Settings => self.finish_setup_later(ws_id, None, ctx),
            ConfigAction::Add => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
            ConfigAction::Remove(key) => {
                info!("[{ws_id}] removing {key}");
                self.apply_after_save(ws_id, DeviceChange::Remove(key), ctx);
            }
            ConfigAction::Configure(cfg) => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_configure(ws_id, *cfg, ctx);
                    }
                });
            }
            ConfigAction::Backup(text) => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.setup = Some(SetupState::Backup);
                        act.request_input(&ws_id, backup_page(&text), ctx);
                    }
                });
            }
            ConfigAction::Restore => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.setup = Some(SetupState::Restore);
                        act.request_input(&ws_id, restore_page(), ctx);
                    }
                });
            }
            ConfigAction::Reset => {
                // The receivers are replaced when the setup ends with a receiver.
                self.setup_replace = true;
                info!("[{ws_id}] configuration reset after the setup");
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
        }
        Ok(())
    }

    /// Show the options of a configured receiver. Like the Python integration, ask the receiver
    /// if it has Dirac. If it does not answer, the saved value is shown. The tuner is asked only
    /// if it was never set.
    fn request_configure(&self, ws_id: String, cfg: DeviceConfig, ctx: &mut Context<Self>) {
        let flow = self.setup_flow;
        let host = cfg.address.clone();
        let ask_tuner = cfg.tuner.is_none();
        ctx.spawn(
            async move { detect_opt(&host, ask_tuner).await }
                .into_actor(self)
                .map(move |detected, act, ctx| {
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    let mut cfg = cfg;
                    if let Some(detected) = detected {
                        cfg.is_dirac_supported = Some(detected.dirac);
                        if ask_tuner && detected.tuner.is_some() {
                            cfg.tuner = detected.tuner;
                        }
                    }
                    act.setup = Some(SetupState::Configure {
                        key: cfg.id.clone(),
                    });
                    let page = configure_page(&cfg);
                    act.request_input(&ws_id, page, ctx);
                }),
        );
    }

    /// Write `config.json` with the change, then change the devices in memory and end the setup.
    /// The write runs off the actix thread. A failed write ends the setup with an error and
    /// changes nothing. A device that was removed during the write is not added again, and the
    /// file is written again from memory. If the setup flow ended during the write, the memory
    /// still follows the file, but no setup event is sent.
    fn apply_after_save(&mut self, ws_id: String, change: DeviceChange, ctx: &mut Context<Self>) {
        let flow = self.setup_flow;
        let (save, writes) = match self.save_change(&change) {
            Ok(save) => save,
            Err(e) => {
                self.finish_setup_later(ws_id, Some(e), ctx);
                return;
            }
        };
        ctx.spawn(save.wait().into_actor(self).map(move |result, act, ctx| {
            let result = match result {
                Ok(()) => act.apply_change(&ws_id, change, writes, ctx),
                Err(e) => {
                    warn!("[{ws_id}] saving the configuration failed: {e}");
                    Err(IntegrationSetupError::Other)
                }
            };
            if act.setup_flow != flow {
                debug!("[{ws_id}] setup flow ended during the write");
                // The end of the old flow did not see the changed devices.
                if !act.devices.is_empty() {
                    let _ =
                        act.sm_consume(&ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
                }
                return;
            }
            // The request got its answer first.
            act.finish_setup_later(ws_id, result.err(), ctx);
        }));
    }

    /// Queue the write of `config.json` with the change. The devices in memory are not changed.
    /// Gives the result of the write and [`ConfigStore::device_writes`] after the write was
    /// queued.
    ///
    /// [`ConfigStore::device_writes`]: crate::config_store::ConfigStore::device_writes
    fn save_change(
        &mut self,
        change: &DeviceChange,
    ) -> Result<(SaveWait, u64), IntegrationSetupError> {
        let configured;
        let mut devices: Vec<&DeviceConfig> = match change {
            DeviceChange::Restore(devices) => devices.iter().collect(),
            DeviceChange::Remove(key) => self
                .devices
                .iter()
                .filter(|(k, _)| ***k != **key)
                .map(|(_, d)| &d.cfg)
                .collect(),
            DeviceChange::Configure {
                key,
                address,
                options,
            } => {
                let device = self
                    .devices
                    .get(key.as_str())
                    .ok_or(IntegrationSetupError::NotFound)?;
                let mut cfg = device.cfg.clone();
                set_options(&mut cfg, address.as_ref(), *options);
                configured = cfg;
                self.devices
                    .iter()
                    .map(|(k, d)| if **k == **key { &configured } else { &d.cfg })
                    .collect()
            }
            DeviceChange::Add { cfg, replace, .. } => {
                let mut devices: Vec<&DeviceConfig> = if *replace {
                    Vec::with_capacity(1)
                } else {
                    self.devices
                        .iter()
                        .filter(|(k, _)| ***k != *cfg.key())
                        .map(|(_, d)| &d.cfg)
                        .collect()
                };
                devices.push(cfg);
                devices
            }
        };
        devices.sort_unstable_by(|a, b| a.cmp_list_order(b));
        let save = self.store.save_devices(&devices).map_err(|e| {
            warn!("Saving the configuration failed: {e}");
            IntegrationSetupError::Other
        })?;
        Ok((save, self.store.device_writes()))
    }

    /// Change the devices in memory after `config.json` was written with the change. The change
    /// is applied to the configuration as it is now: device actors can change it during the
    /// write. `writes` is [`ConfigStore::device_writes`] after the write of the change was
    /// queued. If other writes were queued after it, or if the change failed, the file is written
    /// again from memory.
    ///
    /// [`ConfigStore::device_writes`]: crate::config_store::ConfigStore::device_writes
    fn apply_change(
        &mut self,
        ws_id: &str,
        change: DeviceChange,
        writes: u64,
        ctx: &Context<Self>,
    ) -> Result<(), IntegrationSetupError> {
        let result = match change {
            DeviceChange::Restore(devices) => {
                self.restore_devices(devices, ctx);
                Ok(())
            }
            DeviceChange::Remove(key) => {
                if let Some(cfg) = self.remove_device(&key) {
                    info!("[{ws_id}] removed {}", cfg.name);
                }
                Ok(())
            }
            DeviceChange::Configure {
                key,
                address,
                options,
            } => {
                // A device that was removed during the write is not added again.
                if let Some(device) = self.devices.get_mut(key.as_str()) {
                    set_options(&mut device.cfg, address.as_ref(), options);
                    device.send_config();
                    Ok(())
                } else {
                    Err(IntegrationSetupError::NotFound)
                }
            }
            DeviceChange::Add {
                cfg,
                update,
                replace,
            } => {
                // A device that was removed during the write is not added again.
                if update && !self.devices.contains_key(cfg.key()) {
                    Err(IntegrationSetupError::NotFound)
                } else {
                    if replace {
                        self.remove_other_devices(cfg.key());
                    }
                    if let Some(device) = self.devices.get_mut(cfg.key()) {
                        let old = std::mem::replace(&mut device.cfg, *cfg);
                        if update {
                            take_unused(&mut device.cfg, old);
                        }
                        device.send_config();
                    } else {
                        self.add_device(*cfg, ctx);
                    }
                    Ok(())
                }
            }
        };
        // Entities can come or go, for example when the connection mode changes. New devices
        // connect if a Remote uses them.
        self.purge_entities();
        if result.is_err() || self.store.device_writes() != writes {
            info!("[{ws_id}] writing the configuration again from memory");
            if let Err(e) = self.queue_devices() {
                warn!("[{ws_id}] saving the configuration failed: {e}");
            }
        }
        result
    }

    /// End the setup after the request was acknowledged.
    fn finish_setup_later(
        &self,
        ws_id: String,
        error: Option<IntegrationSetupError>,
        ctx: &mut Context<Self>,
    ) {
        let flow = self.setup_flow;
        ctx.run_later(ACK_DELAY, move |act, ctx| {
            if act.is_setup_flow(flow, &ws_id) {
                act.finish_setup(&ws_id, error, ctx);
            }
        });
    }

    /// Turn the debug log on or off and save the setting.
    fn set_debug_log(&mut self, ws_id: &str, on: bool) {
        if self.driver_settings.debug_log == on {
            return;
        }
        self.driver_settings.debug_log = on;
        logging::set_debug(on);
        if logging::env_filter() {
            info!(
                "[{ws_id}] debug log {}, RUST_LOG is used instead",
                if on { "saved" } else { "off" }
            );
        } else {
            info!("[{ws_id}] debug log {}", if on { "on" } else { "off" });
        }
        if let Err(e) = self.store.save_settings(self.driver_settings) {
            warn!("[{ws_id}] saving the settings failed: {e}");
        }
    }

    /// Remove all devices but the one with `key`, and the cached states of their entities. Memory
    /// only: the caller writes the file.
    fn remove_other_devices(&mut self, key: &str) {
        let keys: Vec<Arc<str>> = self
            .devices
            .keys()
            .filter(|k| ***k != *key)
            .cloned()
            .collect();
        for key in &keys {
            self.remove_device(key);
        }
    }

    /// Replace all devices with the devices of a backup. Memory only: the caller writes the file.
    fn restore_devices(&mut self, devices: Vec<DeviceConfig>, ctx: &Context<Self>) {
        let keys: Vec<Arc<str>> = self.devices.keys().cloned().collect();
        for key in &keys {
            self.remove_device(key);
        }
        for cfg in devices {
            self.add_device(cfg, ctx);
        }
    }

    /// Show a setup page.
    fn request_input(&mut self, ws_id: &str, page: Value, ctx: &mut Context<Self>) {
        self.send_input_request(ws_id, page, None, ctx);
    }

    /// Show the page of the current step again, with an error message.
    fn request_input_again(&mut self, ws_id: &str, message: &str, ctx: &mut Context<Self>) {
        let page = match &self.setup {
            Some(SetupState::Discover) => Some(discover_page()),
            Some(SetupState::RestoreChoice) => Some(restore_choice_page(self.driver_settings)),
            Some(SetupState::Restore) => Some(restore_page()),
            Some(SetupState::Backup) => backup_json(&sorted_configs(&self.devices))
                .as_deref()
                .map(backup_page),
            Some(SetupState::ConfigMode) => Some(self.config_mode_page()),
            Some(SetupState::ChooseDevice { found }) => Some(choose_device_page(found)),
            Some(SetupState::Configure { key }) => self
                .devices
                .get(key.as_str())
                .map(|d| configure_page(&d.cfg)),
            None => None,
        };
        match page {
            Some(page) => self.send_input_request(ws_id, page, Some(message), ctx),
            None => self.finish_setup(ws_id, Some(IntegrationSetupError::Other), ctx),
        }
    }

    /// Send a setup page. With `error`, the page is shown again because of invalid input.
    fn send_input_request(
        &mut self,
        ws_id: &str,
        page: Value,
        error: Option<&str>,
        ctx: &mut Context<Self>,
    ) {
        if self
            .sm_consume(ws_id, &operation_mode::Input::RequestUserInput, ctx)
            .is_err()
        {
            return;
        }
        let mut event = json!({
            "event_type": SetupChangeEventType::Setup,
            "state": IntegrationSetupState::WaitUserAction,
            "require_user_action": { "input": page },
        });
        if let Some(message) = error {
            event["error"] = json!(IntegrationSetupError::InvalidInput);
            event["error_message"] = json!({ "en": message });
        }
        self.send_r2_msg(
            WsMessage::event("driver_setup_change", EventCategory::Device, event),
            ws_id,
        );
    }

    /// End the setup flow.
    fn finish_setup(
        &mut self,
        ws_id: &str,
        error: Option<IntegrationSetupError>,
        ctx: &mut Context<Self>,
    ) {
        self.end_setup_flow();
        let input = if error.is_none() {
            operation_mode::Input::Successful
        } else {
            operation_mode::Input::SetupError
        };
        if self.sm_consume(ws_id, &input, ctx).is_err() {
            return;
        }
        if let Some(session) = self.sessions.get_mut(ws_id) {
            session.reconfiguring = None;
        }
        // Configured devices keep working after a failed setup.
        if error.is_some() && !self.devices.is_empty() {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
        }
        if matches!(self.machine.state(), operation_mode::State::Running) {
            self.update_connections(ConnectTrigger::Wake);
        }
        let state = if error.is_none() {
            IntegrationSetupState::Ok
        } else {
            IntegrationSetupState::Error
        };
        info!("[{ws_id}] setup flow finished: {state:?}");
        match serde_json::to_value(DriverSetupChange {
            event_type: SetupChangeEventType::Stop,
            state,
            error,
            error_message: None,
            require_user_action: None,
        }) {
            Ok(event) => self.send_r2_msg(
                WsMessage::event("driver_setup_change", EventCategory::Device, event),
                ws_id,
            ),
            Err(e) => warn!("driver_setup_change serialize error: {e}"),
        }
    }

    fn config_mode_page(&self) -> Value {
        let devices = sorted_configs(&self.devices);
        let mut items: Vec<Value> = devices
            .iter()
            .map(|c| json!({ "id": c.id, "label": { "en": format!("{} ({} - {})", c.name, c.id, c.shown_address()) } }))
            .collect();
        let mut actions = vec![json!({ "id": "add", "label": { "en": "Add a new device" } })];
        if items.is_empty() {
            items.push(json!({ "id": "", "label": { "en": "---" } }));
        } else {
            actions.extend([
                json!({ "id": "remove", "label": { "en": "Delete selected device" } }),
                json!({ "id": "configure", "label": { "en": "Configure selected device" } }),
                json!({ "id": "reset", "label": { "en": "Reset configuration and reconfigure" } }),
                json!({ "id": "backup", "label": { "en": "Backup configuration" } }),
            ]);
        }
        actions.extend([
            json!({ "id": "restore", "label": { "en": "Restore configuration from backup" } }),
            json!({ "id": "settings", "label": { "en": "Save settings" } }),
        ]);
        let first = devices.first().map_or_default(|c| c.id.as_str());
        json!({
            "title": { "en": "Configuration mode" },
            "settings": [
                {
                    "id": "choice",
                    "label": { "en": "Configured devices" },
                    "field": { "dropdown": { "value": first, "items": items } }
                },
                {
                    "id": "action",
                    "label": { "en": "Action" },
                    "field": { "dropdown": { "value": "add", "items": actions } }
                },
                debug_log_field(self.driver_settings.debug_log)
            ]
        })
    }
}

/// Shown for invalid restore data. It must not contain the data.
const RESTORE_FAILED: &str = "Restore failed or invalid backup data. Please try again.";

fn restore_choice_page(settings: DriverSettings) -> Value {
    json!({
        "title": { "en": "Restore" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "Integration upgrade" },
                "field": { "label": { "value": { "en": "Do you have a configuration backup? Restore it now. Otherwise, continue to add a new device.\n\nYou can make a backup later: run the setup again and choose _Backup configuration_." } } }
            },
            {
                "id": "restore_from_backup",
                "label": { "en": "Restore from backup" },
                "field": { "checkbox": { "value": false } }
            },
            debug_log_field(settings.debug_log)
        ]
    })
}

fn restore_page() -> Value {
    json!({
        "title": { "en": "Restore configuration" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "Restore configuration" },
                "field": { "label": { "value": { "en": "Paste the backup below. It replaces all configured devices." } } }
            },
            {
                "id": "restore_data",
                "label": { "en": "Backup data" },
                "field": { "password": { "value": "" } }
            }
        ]
    })
}

fn backup_page(text: &str) -> Value {
    json!({
        "title": { "en": "Backup configuration" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "Backup" },
                "field": { "label": { "value": { "en": "Copy the data below and keep it in a safe place. It contains the settings of your receivers." } } }
            },
            {
                "id": "backup_data",
                "label": { "en": "Backup data" },
                "field": { "textarea": { "value": text } }
            }
        ]
    })
}

fn discover_page() -> Value {
    json!({
        "title": { "en": "Setup mode" },
        "settings": [
            {
                "id": "info",
                "label": { "en": "" },
                "field": { "label": { "value": { "en": "Leave empty to search the network and click _Next_.\n\nThe search finds receivers only on the same network as the Remote. Enter the address of a receiver on another network." } } }
            },
            {
                "id": "address",
                "label": { "en": "IP address or hostname (optional)" },
                "field": { "text": { "value": "" } }
            }
        ]
    })
}

fn debug_log_field(value: bool) -> Value {
    json!({
        "id": "debug_log",
        "label": { "en": "Debug log (for error reports, more log lines)" },
        "field": { "checkbox": { "value": value } }
    })
}

fn device_label(f: &Found) -> String {
    let address = f.host.as_deref().unwrap_or(&f.address);
    format!("{} ({} - {address})", f.name, f.model)
}

fn choose_device_page(found: &[Found]) -> Value {
    let items: Vec<Value> = found
        .iter()
        .map(|f| json!({ "id": f.address, "label": { "en": device_label(f) } }))
        .collect();
    let first = &found[0];
    let mut settings = vec![json!({
        "id": "choice",
        "label": { "en": "Choose your Denon or Marantz AVR" },
        "field": { "dropdown": { "value": first.address, "items": items } }
    })];
    settings.extend(Options::for_found(first).fields());
    json!({
        "title": { "en": "Please choose your Denon or Marantz AVR" },
        "settings": settings
    })
}

fn configure_page(cfg: &DeviceConfig) -> Value {
    let mut settings = vec![json!({
        "id": "address",
        "label": { "en": "IP address or hostname (leave unchanged to keep it)" },
        "field": { "text": { "value": cfg.shown_address() } }
    })];
    settings.extend(Options::of(cfg).fields());
    json!({
        "title": { "en": "Configure your AVR" },
        "settings": settings
    })
}

/// The receivers at one address, or all that answer the search.
async fn find_receivers(address: Option<String>) -> Result<Vec<Found>, AvrError> {
    if let Some(entered) = address {
        let EnteredAvr { avr, at } = identify_entered(entered, ENTERED_TIMEOUT).await?;
        let id = discovery::Identified::new(&avr).map_err(|reason| {
            AvrError::InvalidRequest(format!("[{}] not used: {reason}", at.address))
        })?;
        let tuner = detect_tuner(&avr).await;
        return Ok(vec![Found::from_identified(at, &id, tuner)]);
    }
    let receivers = discovery::discover().await?;
    // The search does not say if the receiver has Dirac or a tuner: ask each one.
    let detected = futures::future::join_all(receivers.iter().map(|r| detect(&r.host))).await;
    Ok(receivers
        .iter()
        .zip(detected)
        .map(|(r, d)| Found::new(r, d))
        .collect())
}

/// A receiver that answered at an entered address.
struct EnteredAvr {
    avr: Avr,
    /// The address that answered, and the entered host name.
    at: Entered,
}

/// Identify the receiver at an entered address. A host name is resolved: the first of its IP
/// addresses that answers is used. If it cannot be resolved, the receiver is identified by the
/// name. If no address answers, the error of the last one. A host name gets `limit` for all of
/// it, the resolve included. After that, the error of the last address that failed, or a
/// timeout.
async fn identify_entered(entered: String, limit: Duration) -> Result<EnteredAvr, AvrError> {
    if is_ip(&entered) {
        let avr = discovery::identify(&entered, None).await?;
        return Ok(EnteredAvr {
            avr,
            at: Entered::ip(entered),
        });
    }
    let deadline = Instant::now() + limit;
    let mut last_error = None;
    let answered = tokio::time::timeout_at(deadline, async {
        let ips = resolve_name(&entered).await;
        if ips.is_empty() {
            let result = discovery::identify(&entered, None).await;
            return Some(result.map(|avr| (avr, entered.clone())));
        }
        for ip in ips {
            match discovery::identify(&ip, None).await {
                Ok(avr) => return Some(Ok((avr, ip))),
                Err(e) => {
                    debug!("[{entered}] address {ip} failed: {e}");
                    last_error = Some(e);
                }
            }
        }
        None
    })
    .await;
    let (avr, address) = match answered {
        Ok(Some(result)) => result?,
        Ok(None) => return Err(last_error.unwrap_or_else(|| no_answer(&entered, limit))),
        Err(_) => {
            info!("[{entered}] no answer within {limit:?}");
            return Err(last_error.unwrap_or_else(|| no_answer(&entered, limit)));
        }
    };
    Ok(EnteredAvr {
        avr,
        at: Entered {
            address,
            host: Some(entered),
        },
    })
}

/// The error of an entered host name that got no answer within `limit`.
fn no_answer(entered: &str, limit: Duration) -> AvrError {
    AvrError::Timeout {
        request: format!("identify {entered}"),
        limit,
        sent: true,
    }
}

/// Time to wait for the answer of a receiver to the Dirac question.
const DETECT_TIMEOUT: Duration = Duration::from_secs(2);

/// What the setup asks the receiver for its option defaults.
#[derive(Debug, Clone, Copy)]
struct Detected {
    dirac: bool,
    /// `None` if it could not be found out.
    tuner: Option<bool>,
    /// See `DeviceConfig::has_signal_check`. True if it could not be found out.
    signal_check: bool,
}

/// Ask the receiver if it supports Dirac Live and has a tuner. Without an answer: no Dirac, and
/// the tuner is not known.
async fn detect(host: &str) -> Detected {
    detect_opt(host, true).await.unwrap_or(Detected {
        dirac: false,
        tuner: None,
        signal_check: true,
    })
}

/// `None` if the receiver does not answer. The tuner is not checked without `ask_tuner`.
async fn detect_opt(host: &str, ask_tuner: bool) -> Option<Detected> {
    let avr = tokio::time::timeout(DETECT_TIMEOUT, discovery::identify(host, None))
        .await
        .ok()?
        .ok()?;
    let tuner = if ask_tuner {
        detect_tuner(&avr).await
    } else {
        None
    };
    Some(Detected {
        dirac: avr.info().dirac_live,
        tuner,
        signal_check: signal_check(avr.info()),
    })
}

/// Telnet checks the signal itself (`CommApiVers` 0301 or higher), or the version is not known.
/// The same rule as `DeviceConfig::has_signal_check` for the stored version: a receiver without
/// `CommApiVers` has none, and a version that cannot be read is not known.
fn signal_check(info: &denon_marantz::Info) -> bool {
    discovery::receiver_kind(info).comm_api_version.is_none() || info.has_heos()
}

/// The default of the tuner option: from `Deviceinfo.xml`, else from the telnet check while the
/// receiver is on. `None` if it is not known: the user chooses.
async fn detect_tuner(avr: &Avr) -> Option<bool> {
    match avr.info().tuner {
        TunerInfo::Present(_) => Some(true),
        TunerInfo::Absent => Some(false),
        _ => discovery::probe_tuner(&avr.info().host, discovery::TELNET_PORT).await,
    }
}

/// Read the id and the name of the receiver at an address.
async fn identify_host(host: &str) -> Result<discovery::Identified, IntegrationSetupError> {
    let avr = discovery::identify(host, None)
        .await
        .map_err(|e| setup_error(&e))?;
    discovery::Identified::new(&avr).map_err(|reason| {
        warn!("[{host}] not used: {reason}");
        IntegrationSetupError::Other
    })
}

/// Address of a setup page. An empty text is no address. An IPv6 address is stored without
/// brackets: the telnet check of the tuner and the HEOS player search do not accept them.
fn parse_address(value: Option<&str>) -> Result<Option<String>, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) if v.contains(char::is_whitespace) => Err(format!("Invalid address: {v}")),
        Some(v) => match v.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            Some("") => Err(format!("Invalid address: {v}")),
            Some(bare) => Ok(Some(bare.to_owned())),
            None => Ok(Some(v.to_owned())),
        },
    }
}

fn parse_bool(values: &HashMap<String, String>, key: &str, default: bool) -> bool {
    values.get(key).map_or(default, |v| v == "true")
}

const fn setup_error(e: &AvrError) -> IntegrationSetupError {
    if e.is_timeout() {
        IntegrationSetupError::Timeout
    } else if e.is_unreachable() || e.is_network() {
        IntegrationSetupError::ConnectionRefused
    } else {
        IntegrationSetupError::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(None, parse_address(None).expect("ok"));
        assert_eq!(None, parse_address(Some(" ")).expect("ok"));
        assert_eq!(
            Some("192.0.2.2".to_owned()),
            parse_address(Some(" 192.0.2.2 ")).expect("ok")
        );
        assert_eq!(
            Some("avr.local".to_owned()),
            parse_address(Some("avr.local")).expect("ok")
        );
        assert!(parse_address(Some("a b")).is_err());
        assert_eq!(
            Some("fe80::1".to_owned()),
            parse_address(Some("[fe80::1]")).expect("ok")
        );
        assert_eq!(
            Some("fe80::1".to_owned()),
            parse_address(Some("fe80::1")).expect("ok")
        );
        assert!(parse_address(Some("[]")).is_err());
    }

    fn options() -> Options {
        Options {
            show_all_inputs: false,
            is_denon: true,
            use_telnet: true,
            is_dirac_supported: false,
            has_tuner: true,
            tuner_unknown: false,
            signal_poll_secs: 0,
            signal_check: true,
        }
    }

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn options_are_read() {
        let o = options()
            .read(&values(&[
                ("manufacturer", "marantz"),
                ("show_all_inputs", "true"),
                ("connection_mode", "use_http"),
                ("is_dirac_supported", "True"),
                ("has_tuner", "False"),
                ("signal_poll_secs", "10"),
            ]))
            .expect("ok");
        assert!(!o.is_denon && o.show_all_inputs && !o.use_telnet && o.is_dirac_supported);
        assert!(!o.has_tuner);
        assert_eq!(10, o.signal_poll_secs);
        // Missing fields keep the values.
        assert_eq!(options(), options().read(&HashMap::new()).expect("ok"));
    }

    #[test]
    fn the_tuner_field_asks_when_the_setup_could_not_find_it_out() {
        let field = |has_tuner| {
            let found = Found {
                address: "h".into(),
                host: None,
                name: "N".into(),
                model: "M".into(),
                serial: None,
                mac: None,
                is_denon: true,
                is_dirac_supported: false,
                has_tuner,
                signal_check: true,
            };
            Options::for_found(&found)
                .fields()
                .into_iter()
                .find(|f| f["id"] == "has_tuner")
                .expect("tuner field")
        };
        let unknown = field(None);
        assert!(
            unknown["label"]["en"]
                .as_str()
                .is_some_and(|l| l.contains("not found out"))
        );
        assert_eq!("True", unknown["field"]["dropdown"]["value"]);
        let none = field(Some(false));
        assert_eq!("Receiver has an AM/FM tuner", none["label"]["en"]);
        assert_eq!("False", none["field"]["dropdown"]["value"]);
    }

    #[test]
    fn the_signal_field_shows_only_with_the_check_of_the_library() {
        let has_field = |signal_check| {
            Options {
                signal_check,
                ..options()
            }
            .fields()
            .iter()
            .any(|f| f["id"] == "signal_poll_secs")
        };
        assert!(has_field(true));
        assert!(!has_field(false));
        // The note stays last.
        let fields = options().fields();
        assert_eq!(Some("info"), fields.last().and_then(|f| f["id"].as_str()));
    }

    #[test]
    fn the_signal_field_comes_from_the_configuration_and_the_search() {
        let of =
            |json: &str| Options::of(&serde_json::from_str::<DeviceConfig>(json).expect("json"));
        let old = of(r#"{"id":"A","name":"N","address":"h","comm_api_version":"0300"}"#);
        assert!(!old.signal_check);
        let low = of(r#"{"id":"A","name":"N","address":"h","signal_poll_secs":1}"#);
        assert!(low.signal_check);
        assert_eq!(MIN_SIGNAL_SECS, low.signal_poll_secs);
        let high = of(r#"{"id":"A","name":"N","address":"h","signal_poll_secs":60000}"#);
        assert_eq!(MAX_SIGNAL_POLL_SECS, high.signal_poll_secs);
        let found = Found {
            address: "h".into(),
            host: None,
            name: "N".into(),
            model: "M".into(),
            serial: None,
            mac: None,
            is_denon: true,
            is_dirac_supported: false,
            has_tuner: None,
            signal_check: false,
        };
        let options = Options::for_found(&found);
        assert_eq!(DEFAULT_SIGNAL_SECS, options.signal_poll_secs);
        assert!(!options.signal_check);
    }

    #[test]
    fn the_search_uses_the_signal_check_rule_of_the_configuration() {
        let check = |version: Option<&str>| {
            let cfg: DeviceConfig =
                serde_json::from_str(r#"{"id":"A","name":"N","address":"h"}"#).expect("json");
            let mut info = crate::config_store::read_with_version(cfg, "0301")
                .info
                .expect("info");
            info.comm_api_version = version.map(str::to_owned);
            signal_check(&info)
        };
        assert!(check(Some("0301")));
        assert!(!check(Some("0300")));
        // A read without `CommApiVers` is an old receiver; a version that cannot be read is not
        // known.
        assert!(!check(None));
        assert!(check(Some("abc")));
    }

    #[test]
    fn the_signal_time_is_0_or_at_least_the_lowest_time() {
        let read = |value| {
            options()
                .read(&values(&[("signal_poll_secs", value)]))
                .map(|o| o.signal_poll_secs)
        };
        assert_eq!(Ok(0), read("0"));
        assert_eq!(Ok(MIN_SIGNAL_SECS), read(" 3 "));
        assert_eq!(Ok(DEFAULT_SIGNAL_SECS), read(""));
        assert_eq!(Ok(MAX_SIGNAL_POLL_SECS), read("3600"));
        for value in ["1", "2", "3601"] {
            assert!(read(value).is_err(), "{value}");
        }
    }

    #[test]
    fn invalid_options_are_rejected() {
        for (key, value) in [("signal_poll_secs", "-1"), ("signal_poll_secs", "99999")] {
            assert!(
                options().read(&values(&[(key, value)])).is_err(),
                "{key}={value}"
            );
        }
    }

    mod flow {
        use crate::config_store::DeviceConfig;
        use crate::config_store::backup_json;
        use crate::controller::handler::setup::{
            DeviceChange, Entered, Options, ReceiverIds, SetDriverUserDataMsg, SetupDriverMsg,
            TEST_RESOLVE, identify_entered,
        };
        use crate::controller::test_util::{
            DeviceAddr, FlushStore, Subscribed, SubscribesAll, TestRemote, config_file, connect,
            controller, settle, test_dir, use_closed_port, wait_stopped,
        };
        use crate::controller::{Controller, R2RequestMsg, R2SessionDisconnect};
        use actix::Addr;
        use serde_json::{Value, json};
        use std::time::Duration;
        use uc_api::intg::IntegrationSetup;
        use uc_api::intg::ws::R2Request;

        fn values(ws_id: &str, values: &[(&str, &str)]) -> SetDriverUserDataMsg {
            let values = values
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect();
            SetDriverUserDataMsg {
                ws_id: ws_id.into(),
                data: IntegrationSetup::InputValues(values),
                ack: None,
            }
        }

        fn start_setup(ws_id: &str, reconfigure: bool) -> SetupDriverMsg {
            let data =
                serde_json::from_value(json!({ "reconfigure": reconfigure, "setup_data": {} }))
                    .expect("setup");
            SetupDriverMsg {
                ws_id: ws_id.into(),
                data,
                ack: None,
            }
        }

        /// The last setup event.
        fn last(events: &TestRemote) -> Value {
            events.last("driver_setup_change")
        }

        async fn pause() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        /// Wait until the last event has this setup state. The configuration is saved off the
        /// actix thread, so the end of a setup can take longer than [`pause`] on a slow machine.
        async fn wait_state(events: &TestRemote, state: &str) {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while last(events)["state"] != state {
                assert!(
                    std::time::Instant::now() < end,
                    "state {} instead of {state}",
                    last(events)["state"]
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        const PYTHON_CONFIG: &str = r#"[{"id": "SERIAL0000001", "name": "AVR", "address": "192.0.2.6", "support_sound_mode": true, "show_all_inputs": false, "use_telnet": true, "update_audyssey": false, "zone2": false, "zone3": false, "volume_step": 1.0, "timeout": 2000, "is_denon": true, "is_dirac_supported": true}]"#;

        /// The ids of the available entities.
        async fn entity_ids(controller: &Addr<Controller>) -> Vec<String> {
            let msg = R2RequestMsg {
                ws_id: "a".into(),
                req_id: 1,
                request: R2Request::GetAvailableEntities,
                msg_data: None,
            };
            let response = controller
                .send(msg)
                .await
                .expect("send")
                .expect("ok")
                .expect("response");
            response.msg_data.expect("data")["available_entities"]
                .as_array()
                .expect("list")
                .iter()
                .filter_map(|e| e["entity_id"].as_str().map(str::to_owned))
                .collect()
        }

        /// Send a request of the Remote `a` and give its response. `None` if the controller sent
        /// the response to the Remote.
        async fn request(
            controller: &Addr<Controller>,
            request: R2Request,
            msg_data: Option<Value>,
        ) -> Result<Option<uc_api::ws::WsMessage>, crate::errors::ServiceError> {
            crate::controller::test_util::request(controller, "a", request, msg_data).await
        }

        #[actix::test]
        async fn entity_requests_work_during_a_setup() {
            use_closed_port();
            let controller = controller("requests-in-setup", Some(PYTHON_CONFIG));
            connect(&controller, "a").await;
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            assert_eq!(71, entity_ids(&controller).await.len());
        }

        #[actix::test]
        async fn an_aborted_first_setup_keeps_the_receivers() {
            use_closed_port();
            let controller = controller("abort-first-setup", Some(PYTHON_CONFIG));
            connect(&controller, "a").await;
            controller
                .send(start_setup("a", false))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(crate::controller::handler::AbortDriverSetup {
                    ws_id: "a".into(),
                    timeout: false,
                })
                .await
                .expect("send");
            pause().await;
            assert_eq!(71, entity_ids(&controller).await.len());
        }

        #[actix::test]
        async fn the_filter_gives_only_one_entity_type() {
            use_closed_port();
            let controller = controller("filter", Some(PYTHON_CONFIG));
            connect(&controller, "a").await;
            let filter = json!({ "filter": { "entity_type": "media_player" } });
            let response = request(&controller, R2Request::GetAvailableEntities, Some(filter))
                .await
                .expect("ok")
                .expect("response");
            let data = response.msg_data.expect("data");
            assert_eq!(json!("media_player"), data["filter"]["entity_type"]);
            assert_eq!(
                1,
                data["available_entities"].as_array().expect("list").len()
            );
        }

        #[actix::test]
        async fn subscribe_and_unsubscribe_without_ids_mean_all_entities() {
            use_closed_port();
            let controller = controller("subscribe-all", Some(PYTHON_CONFIG));
            connect(&controller, "a").await;
            let subscribes_all = async || controller.send(SubscribesAll("a")).await.expect("send");
            request(&controller, R2Request::SubscribeEvents, Some(json!({})))
                .await
                .expect("subscribe");
            assert!(subscribes_all().await);
            // An empty list unsubscribes nothing.
            let none = json!({ "entity_ids": [] });
            request(&controller, R2Request::UnsubscribeEvents, Some(none))
                .await
                .expect("unsubscribe");
            assert!(subscribes_all().await);
            request(&controller, R2Request::UnsubscribeEvents, Some(json!({})))
                .await
                .expect("unsubscribe");
            assert!(!subscribes_all().await);
            assert_eq!(0, controller.send(Subscribed("a")).await.expect("send"));
            // An empty list subscribes all entities.
            let all = json!({ "entity_ids": [] });
            request(&controller, R2Request::SubscribeEvents, Some(all))
                .await
                .expect("subscribe");
            assert!(subscribes_all().await);
        }

        #[actix::test]
        async fn a_python_configuration_gives_the_python_entities() {
            use_closed_port();
            let controller = controller("python-config", Some(PYTHON_CONFIG));
            connect(&controller, "a").await;
            let ids = entity_ids(&controller).await;
            // The 42 entities of the Python integration (media player, remote, 11 selects and 29
            // sensors), and 29 more: 6 selects, 6 switches, 2 numbers and 15 sensors. Before the
            // first connect `CommApiVers` and the tuner are not known, so the 12 sensors of older
            // receivers and the network favorite and tuner preset selects are there too.
            assert_eq!(71, ids.len());
            for id in [
                "media_player.SERIAL0000001",
                "remote.SERIAL0000001",
                "select.dirac_filter.SERIAL0000001",
                "sensor.video_hdmi_signal_in.SERIAL0000001",
            ] {
                assert!(ids.iter().any(|i| i == id), "{id}");
            }
        }

        #[actix::test]
        async fn invalid_input_shows_the_page_again() {
            let controller = controller("invalid-input", None);
            let events = connect(&controller, "a").await;
            connect(&controller, "b").await;
            controller
                .send(start_setup("a", false))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            assert_eq!(json!("WAIT_USER_ACTION"), last(&events)["state"]);
            assert_eq!(
                json!("restore_from_backup"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );

            // Another Remote cannot send input.
            assert!(
                controller
                    .send(values("b", &[("address", "")]))
                    .await
                    .expect("send")
                    .is_err()
            );

            // Continue without a restore.
            controller
                .send(values("a", &[("restore_from_backup", "false")]))
                .await
                .expect("send")
                .expect("ok");
            pause().await;
            assert_eq!(
                json!("address"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );

            // An address with a space is not valid.
            controller
                .send(values("a", &[("address", "not an address")]))
                .await
                .expect("send")
                .expect("ok");
            pause().await;
            let event = last(&events);
            assert_eq!(json!("WAIT_USER_ACTION"), event["state"]);
            assert_eq!(json!("INVALID_INPUT"), event["error"]);
            assert!(event["require_user_action"]["input"].is_object());

            // The setup still waits for input.
            controller
                .send(values("a", &[("address", "also not valid")]))
                .await
                .expect("send")
                .expect("ok");

            // A disconnect of the owner ends the setup, so another Remote can start one.
            controller
                .send(R2SessionDisconnect { id: "a".into() })
                .await
                .expect("send");
            controller
                .send(start_setup("b", false))
                .await
                .expect("send")
                .expect("setup started");
        }

        #[actix::test]
        async fn invalid_restore_data_is_rejected_without_echo() {
            let controller = controller("restore", None);
            let events = connect(&controller, "a").await;
            controller
                .send(start_setup("a", false))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;

            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            pause().await;
            assert_eq!(
                json!("restore_data"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );

            controller
                .send(values("a", &[("restore_data", "secret-not-a-backup")]))
                .await
                .expect("send")
                .expect("ok");
            pause().await;
            let event = last(&events);
            assert_eq!(json!("INVALID_INPUT"), event["error"]);
            assert!(!event.to_string().contains("secret-not-a-backup"));
            assert_eq!(
                json!("restore_data"),
                event["require_user_action"]["input"]["settings"][1]["id"]
            );
        }

        #[actix::test]
        async fn restore_replaces_the_receivers() {
            use_closed_port();
            let controller = controller("restore-valid", None);
            let events = connect(&controller, "a").await;
            let devices: Vec<DeviceConfig> = serde_json::from_str(PYTHON_CONFIG).expect("devices");
            let backup = backup_json(&devices.iter().collect::<Vec<_>>()).expect("backup");
            controller
                .send(start_setup("a", false))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            pause().await;
            controller
                .send(values("a", &[("restore_data", &backup)]))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            assert_eq!(71, entity_ids(&controller).await.len());
        }

        /// The states that the Remote `a` gets when it asks for them.
        async fn entity_states(controller: &Addr<Controller>, remote: &TestRemote) -> Vec<String> {
            crate::controller::test_util::entity_states(controller, remote).await
        }

        #[actix::test]
        async fn entities_that_go_away_are_forgotten() {
            use_closed_port();
            let controller = controller("purge", Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            pause().await;
            assert_eq!(71, entity_states(&controller, &events).await.len());
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(values(
                    "a",
                    &[("choice", "SERIAL0000001"), ("action", "configure")],
                ))
                .await
                .expect("send")
                .expect("ok");
            // The page comes after the Dirac question. Nothing answers it, so the page shows the
            // saved values.
            wait_page(&events, "connection_mode").await;
            // A configuration without the receiver type, HTTP only: the media player, the remote,
            // 6 selects and 7 sensors.
            controller
                .send(values(
                    "a",
                    &[("address", "192.0.2.6"), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            assert_eq!(15, entity_states(&controller, &events).await.len());
        }

        /// Wait until the last setup event is a page with the field `id`.
        async fn wait_page(events: &TestRemote, id: &str) {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            let has_field = |event: &Value| {
                event["require_user_action"]["input"]["settings"]
                    .as_array()
                    .is_some_and(|fields| fields.iter().any(|f| f["id"] == id))
            };
            while !has_field(&last(events)) {
                assert!(std::time::Instant::now() < end, "no page with {id}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// The answer `req_id` comes before the first setup event.
        fn assert_ack_first(events: &TestRemote, req_id: u32) {
            let ack = events
                .position(|m| m["kind"] == "resp" && m["req_id"] == req_id)
                .expect("ack");
            let event = events
                .position(|m| m["msg"] == "driver_setup_change")
                .expect("event");
            assert!(ack < event, "ack {ack}, event {event}");
        }

        #[actix::test]
        async fn setup_answers_come_before_the_setup_events() {
            use_closed_port();
            let controller = controller("setup-ack", None);
            let events = connect(&controller, "a").await;
            let start = R2RequestMsg {
                ws_id: "a".into(),
                req_id: 5,
                request: R2Request::SetupDriver,
                msg_data: Some(json!({ "reconfigure": false, "setup_data": {} })),
            };
            let answer = controller.send(start).await.expect("send").expect("ok");
            assert!(answer.is_none(), "the controller sends the answer");
            wait_page(&events, "restore_from_backup").await;
            assert_ack_first(&events, 5);

            events.clear();
            let input = R2RequestMsg {
                ws_id: "a".into(),
                req_id: 6,
                request: R2Request::SetDriverUserData,
                msg_data: Some(json!({ "input_values": { "restore_from_backup": "false" } })),
            };
            let answer = controller.send(input).await.expect("send").expect("ok");
            assert!(answer.is_none(), "the controller sends the answer");
            wait_page(&events, "address").await;
            assert_ack_first(&events, 6);
        }

        /// Removes the active setup step.
        #[derive(actix::Message)]
        #[rtype(result = "()")]
        struct ClearSetupStep;

        impl actix::Handler<ClearSetupStep> for Controller {
            type Result = ();
            fn handle(&mut self, _: ClearSetupStep, _: &mut actix::Context<Self>) {
                self.setup = None;
            }
        }

        fn user_data(req_id: u32) -> R2RequestMsg {
            R2RequestMsg {
                ws_id: "a".into(),
                req_id,
                request: R2Request::SetDriverUserData,
                msg_data: Some(json!({ "input_values": { "restore_from_backup": "false" } })),
            }
        }

        #[actix::test]
        async fn user_data_without_a_setup_step_is_answered_and_ends_the_setup() {
            use_closed_port();
            let controller = controller("setup-no-step", None);
            let events = connect(&controller, "a").await;
            controller
                .send(start_setup("a", false))
                .await
                .expect("send")
                .expect("setup started");
            wait_page(&events, "restore_from_backup").await;
            controller.send(ClearSetupStep).await.expect("send");
            events.clear();

            let answer = controller
                .send(user_data(7))
                .await
                .expect("send")
                .expect("ok");
            assert!(answer.is_none(), "the controller sends the answer");
            wait_state(&events, "ERROR").await;
            assert_ack_first(&events, 7);
            assert_eq!(
                events.position(|m| m["kind"] == "resp" && m["req_id"] == 7),
                events.position(|m| m["kind"] == "resp" && m["req_id"] == 7 && m["code"] == 200),
                "the first answer has code 200"
            );
        }

        #[actix::test]
        async fn user_data_without_a_setup_flow_gets_no_acknowledgment() {
            use_closed_port();
            let controller = controller("setup-no-flow", None);
            let events = connect(&controller, "a").await;
            let answer = controller.send(user_data(8)).await.expect("send");
            assert!(answer.is_err(), "{answer:?}");
            settle(&controller, &[&events]).await;
            assert_eq!(None, events.position(|m| m["kind"] == "resp"));
        }

        async fn subscribe(controller: &Addr<Controller>, ws_id: &str, ids: &[String]) {
            let msg = R2RequestMsg {
                ws_id: ws_id.into(),
                req_id: 3,
                request: R2Request::SubscribeEvents,
                msg_data: Some(json!({ "entity_ids": ids })),
            };
            controller.send(msg).await.expect("send").expect("ok");
        }

        #[actix::test]
        async fn a_new_session_gets_the_states_only_with_get_entity_states() {
            use_closed_port();
            let controller = controller("subscribe", Some(PYTHON_CONFIG));
            let changes = connect(&controller, "a").await;
            pause().await;
            let (rest, first): (Vec<String>, Vec<String>) = entity_ids(&controller)
                .await
                .into_iter()
                .partition(|id| id.starts_with("media_player."));
            // The start of a session: one event per entity can fill the event queue of the Remote.
            subscribe(&controller, "a", &first).await;
            assert_eq!(0, changes.filter("entity_change").len());
            assert_eq!(
                first.len(),
                entity_states(&controller, &changes).await.len()
            );
            // An entity that is added later gets its state at once.
            subscribe(&controller, "a", &rest).await;
            assert_eq!(1, changes.filter("entity_change").len());
        }

        #[actix::test]
        async fn a_receiver_can_be_removed() {
            use_closed_port();
            let controller = controller("remove", Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            let actor = controller
                .send(DeviceAddr("SERIAL0000001"))
                .await
                .expect("send")
                .expect("actor");
            subscribe(&controller, "a", &["media_player.SERIAL0000001".into()]).await;
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            // The page lists the receiver with its serial number.
            assert_eq!(
                json!("SERIAL0000001"),
                last(&events)["require_user_action"]["input"]["settings"][0]["field"]["dropdown"]["value"]
            );
            controller
                .send(values(
                    "a",
                    &[("choice", "SERIAL0000001"), ("action", "remove")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            assert_eq!(0, entity_ids(&controller).await.len());
            // Only the Remote changes its subscriptions.
            assert_eq!(1, controller.send(Subscribed("a")).await.expect("send"));
            wait_stopped(&controller, &actor).await;
            assert_eq!(json!([]), config_file("remove"));
        }

        /// Make each write of `config.json` of the test `name` fail: the temporary file cannot be
        /// made.
        fn block_config_writes(name: &str) {
            let tmp = test_dir(name).join("config.json.tmp");
            std::fs::create_dir_all(&tmp).expect("dir");
            std::fs::write(tmp.join("x"), b"x").expect("file");
        }

        #[actix::test]
        async fn a_failed_write_of_a_removal_keeps_the_receiver() {
            use_closed_port();
            let controller = controller("remove-write-fails", Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            block_config_writes("remove-write-fails");
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(values(
                    "a",
                    &[("choice", "SERIAL0000001"), ("action", "remove")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "ERROR").await;
            assert_eq!(json!("OTHER"), last(&events)["error"]);
            assert_eq!(71, entity_ids(&controller).await.len());
            let actor = controller
                .send(DeviceAddr("SERIAL0000001"))
                .await
                .expect("send")
                .expect("actor");
            assert!(actor.connected());
            controller.send(FlushStore).await.expect("send");
            assert_eq!(
                json!("SERIAL0000001"),
                config_file("remove-write-fails")[0]["id"]
            );
        }

        #[actix::test]
        async fn a_failed_write_of_the_options_keeps_the_configuration() {
            use_closed_port();
            let controller = controller("configure-write-fails", Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            block_config_writes("configure-write-fails");
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(values(
                    "a",
                    &[("choice", "SERIAL0000001"), ("action", "configure")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_page(&events, "connection_mode").await;
            controller
                .send(values(
                    "a",
                    &[("address", "192.0.2.7"), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "ERROR").await;
            assert_eq!(json!("OTHER"), last(&events)["error"]);
            // HTTP mode would have 15 entities.
            assert_eq!(71, entity_ids(&controller).await.len());
            let cfg = controller
                .send(DeviceCfg("SERIAL0000001"))
                .await
                .expect("send")
                .expect("device");
            assert!(cfg.use_telnet);
            assert_eq!("192.0.2.6", cfg.address);
        }

        /// Open the options of the Python receiver and save them with `address`. The setup must
        /// end with OK. Gives the address that the page showed and the configuration after the
        /// save.
        async fn configure_address(
            controller: &Addr<Controller>,
            events: &TestRemote,
            address: &str,
        ) -> (Value, DeviceConfig) {
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(values(
                    "a",
                    &[("choice", "SERIAL0000001"), ("action", "configure")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_page(events, "connection_mode").await;
            let page = last(events);
            let shown =
                page["require_user_action"]["input"]["settings"][0]["field"]["text"]["value"]
                    .clone();
            controller
                .send(values("a", &[("address", address)]))
                .await
                .expect("send")
                .expect("ok");
            wait_state(events, "OK").await;
            controller.send(FlushStore).await.expect("send");
            let cfg = controller
                .send(DeviceCfg("SERIAL0000001"))
                .await
                .expect("send")
                .expect("device");
            (shown, cfg)
        }

        #[actix::test]
        async fn the_options_store_a_host_name_with_its_ip_address() {
            use_closed_port();
            let name = "configure-host-name";
            let controller = controller(name, Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            let (shown, cfg) = configure_address(&controller, &events, "localhost").await;
            assert_eq!(json!("192.0.2.6"), shown);
            assert_eq!(
                ("127.0.0.1", Some("localhost")),
                (cfg.address.as_str(), cfg.host.as_deref())
            );
            assert_eq!(json!("localhost"), config_file(name)[0]["host"]);
            assert_eq!(json!("127.0.0.1"), config_file(name)[0]["address"]);
            // The page shows the host name. Unchanged, it keeps both.
            let (shown, cfg) = configure_address(&controller, &events, "localhost").await;
            assert_eq!(json!("localhost"), shown);
            assert_eq!(
                ("127.0.0.1", Some("localhost")),
                (cfg.address.as_str(), cfg.host.as_deref())
            );
            // A host name that cannot be resolved is stored as the address too.
            let (_, cfg) = configure_address(&controller, &events, "receiver.invalid").await;
            assert_eq!(
                ("receiver.invalid", Some("receiver.invalid")),
                (cfg.address.as_str(), cfg.host.as_deref())
            );
            // An IP address removes the host name.
            let (_, cfg) = configure_address(&controller, &events, "[::1]").await;
            assert_eq!(("::1", None), (cfg.address.as_str(), cfg.host.as_deref()));
            assert_eq!(None, config_file(name)[0].get("host"));
        }

        #[actix::test]
        async fn invalid_config_mode_input_does_not_change_the_debug_log() {
            use_closed_port();
            let name = "config-mode-debug-log";
            let controller = controller(name, None);
            let events = connect(&controller, "a").await;
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            wait_state(&events, "WAIT_USER_ACTION").await;
            let settings = test_dir(name).join("settings.json");
            for action in ["unknown", "remove", "configure", "backup"] {
                events.clear();
                controller
                    .send(values(
                        "a",
                        &[
                            ("choice", "NOT_CONFIGURED"),
                            ("action", action),
                            ("debug_log", "true"),
                        ],
                    ))
                    .await
                    .expect("send")
                    .expect("ok");
                wait_state(&events, "WAIT_USER_ACTION").await;
                let event = last(&events);
                assert_eq!(json!("INVALID_INPUT"), event["error"], "{action}");
                let field = event["require_user_action"]["input"]["settings"]
                    .as_array()
                    .expect("fields")
                    .iter()
                    .find(|f| f["id"] == "debug_log")
                    .cloned()
                    .expect("debug_log field");
                assert_eq!(
                    json!(false),
                    field["field"]["checkbox"]["value"],
                    "{action}"
                );
                controller.send(FlushStore).await.expect("send");
                assert!(!settings.exists(), "{action}");
            }
            // Valid input saves it.
            controller
                .send(values(
                    "a",
                    &[("action", "settings"), ("debug_log", "true")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            controller.send(FlushStore).await.expect("send");
            let saved: Value =
                serde_json::from_str(&std::fs::read_to_string(&settings).expect("settings"))
                    .expect("json");
            assert_eq!(json!(true), saved["debug_log"]);
        }

        /// The configuration of a device in memory.
        #[derive(actix::Message)]
        #[rtype(result = "Option<DeviceConfig>")]
        struct DeviceCfg(&'static str);

        impl actix::Handler<DeviceCfg> for Controller {
            type Result = Option<DeviceConfig>;
            fn handle(&mut self, msg: DeviceCfg, _: &mut actix::Context<Self>) -> Self::Result {
                self.devices.get(msg.0).map(|d| d.cfg.clone())
            }
        }

        /// Starts the save of `change` and then runs `during` while the write runs.
        #[derive(actix::Message)]
        #[rtype(result = "()")]
        struct ChangeDuringWrite {
            change: DeviceChange,
            during: fn(&mut Controller, &mut actix::Context<Controller>),
        }

        impl actix::Handler<ChangeDuringWrite> for Controller {
            type Result = ();
            fn handle(&mut self, msg: ChangeDuringWrite, ctx: &mut actix::Context<Self>) {
                self.apply_after_save("a".into(), msg.change, ctx);
                (msg.during)(self, ctx);
            }
        }

        /// Set the options of the Python receiver to HTTP mode.
        fn to_http() -> DeviceChange {
            DeviceChange::Configure {
                key: "SERIAL0000001".into(),
                address: Some(Entered::ip("192.0.2.7".into())),
                options: Options {
                    use_telnet: false,
                    ..super::options()
                },
            }
        }

        /// Wait until the device has this configuration in memory, then until the store has
        /// done all queued writes.
        async fn wait_cfg(
            controller: &Addr<Controller>,
            key: &'static str,
            done: impl Fn(Option<&DeviceConfig>) -> bool,
        ) {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while !done(
                controller
                    .send(DeviceCfg(key))
                    .await
                    .expect("send")
                    .as_ref(),
            ) {
                assert!(std::time::Instant::now() < end, "configuration not changed");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            controller.send(FlushStore).await.expect("send");
        }

        #[actix::test]
        async fn the_options_keep_what_changed_during_the_write() {
            use_closed_port();
            let name = "configure-keeps-changes";
            let controller = controller(name, Some(PYTHON_CONFIG));
            controller
                .send(ChangeDuringWrite {
                    change: to_http(),
                    // As a device actor reports names: the controller saves them.
                    during: |act, _| {
                        let device = act.devices.get_mut("SERIAL0000001").expect("device");
                        device.cfg.network_favorite_names = vec!["Radio".into()];
                        act.queue_devices().expect("queue");
                    },
                })
                .await
                .expect("send");
            wait_cfg(&controller, "SERIAL0000001", |cfg| {
                cfg.is_some_and(|c| !c.use_telnet)
            })
            .await;
            let cfg = controller
                .send(DeviceCfg("SERIAL0000001"))
                .await
                .expect("send")
                .expect("device");
            assert_eq!("192.0.2.7", cfg.address);
            assert_eq!(vec!["Radio".to_owned()], cfg.network_favorite_names);
            let file = &config_file(name)[0];
            assert_eq!(json!(false), file["use_telnet"]);
            assert_eq!(json!("192.0.2.7"), file["address"]);
            assert_eq!(json!(["Radio"]), file["network_favorite_names"]);
        }

        #[actix::test]
        async fn the_options_do_not_add_a_receiver_that_was_removed_during_the_write() {
            use_closed_port();
            let name = "configure-removed";
            let controller = controller(name, Some(PYTHON_CONFIG));
            controller
                .send(ChangeDuringWrite {
                    change: to_http(),
                    during: |act, _| {
                        act.remove_device("SERIAL0000001");
                    },
                })
                .await
                .expect("send");
            // The file has the receiver until the change is applied and the file is written
            // again.
            let end = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                controller.send(FlushStore).await.expect("send");
                if config_file(name) == json!([]) {
                    break;
                }
                assert!(std::time::Instant::now() < end, "file not written again");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(
                controller
                    .send(DeviceCfg("SERIAL0000001"))
                    .await
                    .expect("send")
                    .is_none()
            );
        }

        #[actix::test]
        async fn a_change_is_applied_when_the_setup_ends_during_the_write() {
            use_closed_port();
            let name = "remove-after-abort";
            let controller = controller(name, Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(ChangeDuringWrite {
                    change: DeviceChange::Remove("SERIAL0000001".into()),
                    during: |act, ctx| act.abort_setup("a", false, ctx),
                })
                .await
                .expect("send");
            wait_cfg(&controller, "SERIAL0000001", |cfg| cfg.is_none()).await;
            assert_eq!(json!([]), config_file(name));
            assert_eq!(0, entity_ids(&controller).await.len());
            // The aborted flow gets no end event.
            pause().await;
            assert_eq!(json!("WAIT_USER_ACTION"), last(&events)["state"]);
        }

        #[actix::test]
        async fn a_change_of_an_old_flow_does_not_end_a_new_flow() {
            use_closed_port();
            let name = "remove-then-new-setup";
            let controller = controller(name, Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(ChangeDuringWrite {
                    change: DeviceChange::Remove("SERIAL0000001".into()),
                    during: |act, ctx| {
                        act.abort_setup("a", false, ctx);
                        actix::Handler::handle(act, start_setup("a", true), ctx)
                            .expect("setup started");
                    },
                })
                .await
                .expect("send");
            wait_cfg(&controller, "SERIAL0000001", |cfg| cfg.is_none()).await;
            pause().await;
            let states: Vec<Value> = events
                .filter("driver_setup_change")
                .iter()
                .map(|e| e["state"].clone())
                .collect();
            assert!(!states.contains(&json!("OK")), "{states:?}");
            assert_eq!(Some(&json!("WAIT_USER_ACTION")), states.last());
            // The new flow still takes input.
            controller
                .send(values("a", &[("action", "settings")]))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
        }

        /// True if the driver state is `Running`.
        #[derive(actix::Message)]
        #[rtype(result = "bool")]
        struct IsRunning;

        impl actix::Handler<IsRunning> for Controller {
            type Result = bool;
            fn handle(&mut self, _: IsRunning, _: &mut actix::Context<Self>) -> bool {
                is_running(self)
            }
        }

        /// True if the driver state is `Running`.
        fn is_running(controller: &Controller) -> bool {
            matches!(
                controller.machine.state(),
                crate::controller::operation_mode::State::Running
            )
        }

        /// The receiver of [`PYTHON_CONFIG`].
        fn python_device() -> DeviceConfig {
            let mut devices: Vec<DeviceConfig> =
                serde_json::from_str(PYTHON_CONFIG).expect("devices");
            devices.remove(0)
        }

        #[actix::test]
        async fn a_receiver_added_when_the_first_setup_ends_during_the_write_sets_the_driver_running()
         {
            use_closed_port();
            let name = "add-after-abort";
            let controller = controller(name, None);
            connect(&controller, "a").await;
            controller
                .send(start_setup("a", false))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(ChangeDuringWrite {
                    change: DeviceChange::Add {
                        cfg: Box::new(python_device()),
                        update: false,
                        replace: true,
                    },
                    during: |act, ctx| {
                        act.abort_setup("a", false, ctx);
                        assert!(!is_running(act));
                    },
                })
                .await
                .expect("send");
            wait_cfg(&controller, "SERIAL0000001", |cfg| cfg.is_some()).await;
            assert!(controller.send(IsRunning).await.expect("send"));
            assert_eq!(json!("SERIAL0000001"), config_file(name)[0]["id"]);
        }

        #[actix::test]
        async fn a_failed_write_of_a_restore_keeps_the_receivers() {
            use_closed_port();
            let name = "restore-write-fails";
            let controller = controller(name, Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            let mut other = python_device();
            other.id = "OTHER0000001".into();
            let backup = backup_json(&[&other]).expect("backup");
            block_config_writes(name);
            controller
                .send(start_setup("a", false))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            pause().await;
            controller
                .send(values("a", &[("restore_data", &backup)]))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "ERROR").await;
            assert_eq!(json!("OTHER"), last(&events)["error"]);
            assert!(controller_cfg(&controller, "SERIAL0000001").await.is_some());
            assert!(controller_cfg(&controller, "OTHER0000001").await.is_none());
            controller.send(FlushStore).await.expect("send");
            assert_eq!(json!("SERIAL0000001"), config_file(name)[0]["id"]);
        }

        /// The configuration of a device in memory.
        async fn controller_cfg(
            controller: &Addr<Controller>,
            key: &'static str,
        ) -> Option<DeviceConfig> {
            controller.send(DeviceCfg(key)).await.expect("send")
        }

        #[actix::test]
        async fn an_updated_receiver_keeps_the_names_reported_during_the_write() {
            use_closed_port();
            let name = "update-keeps-names";
            let controller = controller(name, Some(PYTHON_CONFIG));
            let mut cfg = python_device();
            cfg.address = "192.0.2.7".into();
            controller
                .send(ChangeDuringWrite {
                    change: DeviceChange::Add {
                        cfg: Box::new(cfg),
                        update: true,
                        replace: false,
                    },
                    during: |act, _| {
                        let device = act.devices.get_mut("SERIAL0000001").expect("device");
                        device.cfg.network_favorite_names = vec!["Radio".into()];
                        act.queue_devices().expect("queue");
                    },
                })
                .await
                .expect("send");
            wait_cfg(&controller, "SERIAL0000001", |cfg| {
                cfg.is_some_and(|c| c.address == "192.0.2.7")
            })
            .await;
            let cfg = controller_cfg(&controller, "SERIAL0000001")
                .await
                .expect("device");
            assert_eq!(vec!["Radio".to_owned()], cfg.network_favorite_names);
            let file = &config_file(name)[0];
            assert_eq!(json!("192.0.2.7"), file["address"]);
            assert_eq!(json!(["Radio"]), file["network_favorite_names"]);
        }

        /// The address of the fake receiver in the setup.
        const FAKE_HOST: &str = "127.0.0.1";

        /// A fake receiver at [`FAKE_HOST`]. Its telnet port does not answer.
        async fn fake_receiver() -> crate::test_support::Fake {
            fake_receiver_from(crate::test_support::Fake::start(true).await)
        }

        /// All receivers on this thread use the fake receiver, also the configured ones. The search
        /// finds nothing.
        fn fake_receiver_from(http: crate::test_support::Fake) -> crate::test_support::Fake {
            let port = http.port();
            let setup: crate::device::discovery::TestBuilder = std::rc::Rc::new(move |_, _| {
                crate::test_support::test_builder(port, 1, denon_marantz::Avr::builder(FAKE_HOST))
            });
            crate::device::discovery::TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
            crate::device::discovery::TEST_DISCOVERY
                .with(|slot| *slot.borrow_mut() = Some(Vec::new()));
            http
        }

        /// Start the setup and give the address of the fake receiver. `reconfigure` keeps the
        /// configured receivers.
        async fn setup_fake(controller: &Addr<Controller>, events: &TestRemote, reconfigure: bool) {
            setup_fake_at(controller, events, reconfigure, FAKE_HOST).await;
        }

        /// As [`setup_fake`], with this address.
        async fn setup_fake_at(
            controller: &Addr<Controller>,
            events: &TestRemote,
            reconfigure: bool,
            address: &str,
        ) {
            controller
                .send(start_setup("a", reconfigure))
                .await
                .expect("send")
                .expect("setup started");
            pause().await;
            let first = if reconfigure {
                ("action", "add")
            } else {
                ("restore_from_backup", "false")
            };
            controller
                .send(values("a", &[first]))
                .await
                .expect("send")
                .expect("ok");
            pause().await;
            controller
                .send(values("a", &[("address", address)]))
                .await
                .expect("send")
                .expect("ok");
            let end = std::time::Instant::now() + Duration::from_secs(5);
            let address_page = |event: &Value| {
                event["require_user_action"]["input"]["settings"][1]["id"] == "address"
            };
            while last(events)["state"] == "SETUP" || address_page(&last(events)) {
                assert!(std::time::Instant::now() < end, "no answer to the address");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        #[actix::test]
        async fn a_new_receiver_gets_its_mac_address_as_id() {
            let _fake = fake_receiver().await;
            let controller = controller("mac-id", None);
            let events = connect(&controller, "a").await;
            setup_fake(&controller, &events, false).await;
            controller
                .send(values(
                    "a",
                    &[("choice", FAKE_HOST), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            let device = &config_file("mac-id")[0];
            assert_eq!(json!("0123456789AB"), device["id"]);
            assert_eq!(json!("mac"), device["id_kind"]);
            assert_eq!(json!("0123456789AB"), device["mac"]);
            assert_eq!(json!("XXX"), device["serial"]);
            // An IP address is stored without a host name.
            assert_eq!(json!(FAKE_HOST), device["address"]);
            assert_eq!(None, device.get("host"));
            assert!(
                entity_ids(&controller)
                    .await
                    .iter()
                    .any(|id| id == "media_player.0123456789AB")
            );
        }

        #[actix::test]
        async fn a_host_name_is_stored_with_its_ip_address() {
            let _fake = fake_receiver().await;
            let controller = controller("host-name", None);
            let events = connect(&controller, "a").await;
            setup_fake_at(&controller, &events, false, "localhost").await;
            // The page names the receiver by the host name, the choice is its IP address.
            let page = last(&events);
            let choice = &page["require_user_action"]["input"]["settings"][0]["field"]["dropdown"];
            assert_eq!(json!(FAKE_HOST), choice["value"]);
            assert!(
                choice["items"][0]["label"]["en"]
                    .as_str()
                    .is_some_and(|l| l.ends_with("- localhost)")),
                "{choice}"
            );
            controller
                .send(values(
                    "a",
                    &[("choice", FAKE_HOST), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            let device = &config_file("host-name")[0];
            assert_eq!(json!(FAKE_HOST), device["address"]);
            assert_eq!(json!("localhost"), device["host"]);
        }

        /// Identify the fake receiver like the setup does. The fake answers for the AVR-X and the
        /// oldest type.
        async fn identify_fake(
            http: crate::test_support::Fake,
        ) -> Result<crate::device::discovery::Identified, &'static str> {
            use denon_marantz::ReceiverType;
            let port = http.port();
            let setup: crate::device::discovery::TestBuilder =
                std::rc::Rc::new(move |_, builder| {
                    crate::test_support::test_builder(port, 1, builder)
                        .probes(&[(ReceiverType::AvrX, port), (ReceiverType::Avr, port)])
                });
            crate::device::discovery::TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
            let avr = crate::device::discovery::identify(FAKE_HOST, None)
                .await
                .expect("identify");
            crate::device::discovery::Identified::new(&avr)
        }

        #[actix::test]
        async fn the_reason_is_given_when_a_receiver_cannot_be_used() {
            use crate::test_support::Fake;
            let reason = |result: Result<_, &'static str>| result.map(|_| ()).unwrap_err();
            assert_eq!(
                "the answers do not prove the receiver type",
                reason(identify_fake(Fake::start_busy().await).await)
            );
            assert_eq!(
                "the receiver did not send its description",
                reason(identify_fake(Fake::start_without_description().await).await)
            );
            assert_eq!(
                "the receiver gave no MAC address",
                reason(identify_fake(Fake::start_without_mac().await).await)
            );
            assert_eq!(
                "the receiver gave no serial number",
                reason(identify_fake(Fake::start_old_with_empty_serial().await).await)
            );
        }

        #[actix::test]
        async fn an_old_receiver_gets_its_serial_number_as_id() {
            let id = identify_fake(crate::test_support::Fake::start_old().await)
                .await
                .expect("identified");
            assert_eq!("XXX", id.id);
            assert_eq!(crate::config_store::IdKind::Serial, id.id_kind);
            assert_eq!(Some("XXX"), id.serial.as_deref());
            assert_eq!(None, id.mac);
        }

        #[actix::test]
        async fn a_receiver_without_its_description_is_not_added() {
            let _fake =
                fake_receiver_from(crate::test_support::Fake::start_without_description().await);
            let controller = controller("no-description", None);
            let events = connect(&controller, "a").await;
            setup_fake(&controller, &events, false).await;
            wait_state(&events, "ERROR").await;
            assert_eq!(json!("OTHER"), last(&events)["error"]);
        }

        #[actix::test]
        async fn a_receiver_without_a_serial_number_is_added_by_its_mac_address() {
            let _fake =
                fake_receiver_from(crate::test_support::Fake::start_with_empty_serial().await);
            let controller = controller("no-serial", None);
            let events = connect(&controller, "a").await;
            setup_fake(&controller, &events, false).await;
            controller
                .send(values(
                    "a",
                    &[("choice", FAKE_HOST), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            let device = &config_file("no-serial")[0];
            assert_eq!(json!("0123456789AB"), device["id"]);
            assert_eq!(None, device.get("serial"));
        }

        /// Choose the fake receiver in HTTP mode and wait for the end of the setup.
        async fn choose_fake(controller: &Addr<Controller>, events: &TestRemote) {
            controller
                .send(values(
                    "a",
                    &[("choice", FAKE_HOST), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(events, "OK").await;
        }

        #[actix::test]
        async fn a_first_setup_replaces_the_receivers_when_it_adds_one() {
            let _fake = fake_receiver().await;
            let controller = controller("first-setup-replaces", Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            setup_fake(&controller, &events, false).await;
            choose_fake(&controller, &events).await;
            let config = config_file("first-setup-replaces");
            assert_eq!(1, config.as_array().expect("list").len());
            assert_eq!(json!("0123456789AB"), config[0]["id"]);
        }

        #[actix::test]
        async fn a_first_setup_offers_a_configured_receiver_and_keeps_its_id() {
            let _fake = fake_receiver().await;
            let config = PYTHON_CONFIG.replace("SERIAL0000001", "XXX");
            let controller = controller("first-setup-keeps-id", Some(&config));
            let events = connect(&controller, "a").await;
            setup_fake(&controller, &events, false).await;
            choose_fake(&controller, &events).await;
            let config = config_file("first-setup-keeps-id");
            assert_eq!(1, config.as_array().expect("list").len());
            assert_eq!(json!("XXX"), config[0]["id"]);
        }

        #[actix::test]
        async fn a_receiver_of_a_python_configuration_is_not_offered_again() {
            let _fake = fake_receiver().await;
            // The serial number of the fake receiver is the id.
            let config = PYTHON_CONFIG.replace("SERIAL0000001", "XXX");
            let controller = controller("known-serial", Some(&config));
            let events = connect(&controller, "a").await;
            setup_fake(&controller, &events, true).await;
            wait_state(&events, "ERROR").await;
            assert_eq!(json!("NOT_FOUND"), last(&events)["error"]);
        }

        #[actix::test]
        async fn a_failed_write_of_an_added_receiver_keeps_the_receivers() {
            let _fake = fake_receiver().await;
            let name = "add-write-fails";
            let controller = controller(name, Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            block_config_writes(name);
            // A first setup: the added receiver would replace the configured one.
            setup_fake(&controller, &events, false).await;
            controller
                .send(values(
                    "a",
                    &[("choice", FAKE_HOST), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "ERROR").await;
            assert_eq!(json!("OTHER"), last(&events)["error"]);
            assert!(controller_cfg(&controller, "SERIAL0000001").await.is_some());
            assert!(controller_cfg(&controller, "0123456789AB").await.is_none());
            controller.send(FlushStore).await.expect("send");
            let config = config_file(name);
            assert_eq!(1, config.as_array().expect("list").len());
            assert_eq!(json!("SERIAL0000001"), config[0]["id"]);
        }

        /// The host name of the tests that resolve names with [`resolve_to`].
        const HOST_NAME: &str = "receiver.test";
        /// The first IP address of [`HOST_NAME`].
        const FIRST_IP: &str = "192.0.2.1";
        /// The second IP address of [`HOST_NAME`].
        const SECOND_IP: &str = "192.0.2.2";

        /// Each host name resolves to these IP addresses on this thread. Empty: no name can be
        /// resolved.
        fn resolve_to(ips: &[&str]) {
            let ips = ips.iter().map(|ip| (*ip).to_owned()).collect();
            TEST_RESOLVE.with(|slot| *slot.borrow_mut() = Some(ips));
        }

        /// The receivers and the setup on this thread connect to the HTTP port `first` for
        /// [`FIRST_IP`], and to `rest` for all other addresses. The search finds nothing.
        fn ports_by_address(first: u16, rest: u16) {
            let setup: crate::device::discovery::TestBuilder = std::rc::Rc::new(move |host, _| {
                let port = if host == FIRST_IP { first } else { rest };
                crate::test_support::test_builder(port, 1, denon_marantz::Avr::builder(FAKE_HOST))
            });
            crate::device::discovery::TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
            crate::device::discovery::TEST_DISCOVERY
                .with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        }

        /// Choose the receiver at `address` in HTTP mode, wait for the end of the setup and give
        /// the saved receiver.
        async fn choose_at(
            controller: &Addr<Controller>,
            events: &TestRemote,
            name: &str,
            address: &str,
        ) -> Value {
            controller
                .send(values(
                    "a",
                    &[("choice", address), ("connection_mode", "use_http")],
                ))
                .await
                .expect("send")
                .expect("ok");
            wait_state(events, "OK").await;
            controller.send(FlushStore).await.expect("send");
            config_file(name)[0].clone()
        }

        #[actix::test]
        async fn the_options_skip_another_receiver_at_the_first_address_of_a_host_name() {
            let receiver = crate::test_support::Fake::start(true).await;
            let other = crate::test_support::Fake::start_with_info(
                b"<Device_Info><CommApiVers>0301</CommApiVers><ModelName>AVC-X3700H</ModelName>\
                <DeviceZones>2</DeviceZones><MacAddress>0123456789AC</MacAddress></Device_Info>",
            )
            .await;
            ports_by_address(other.port(), receiver.port());
            resolve_to(&[FIRST_IP, SECOND_IP]);
            // The MAC address of `receiver`.
            let config = PYTHON_CONFIG.replace(r#""name""#, r#""mac": "0123456789AB", "name""#);
            let controller = controller("configure-other-first", Some(&config));
            let events = connect(&controller, "a").await;
            let (_, cfg) = configure_address(&controller, &events, HOST_NAME).await;
            assert_eq!(
                (SECOND_IP, Some(HOST_NAME)),
                (cfg.address.as_str(), cfg.host.as_deref())
            );
        }

        #[actix::test]
        async fn a_host_name_that_cannot_be_resolved_is_added_by_the_name() {
            let _fake = fake_receiver().await;
            resolve_to(&[]);
            let name = "add-name-not-resolved";
            let controller = controller(name, None);
            let events = connect(&controller, "a").await;
            setup_fake_at(&controller, &events, false, HOST_NAME).await;
            let page = last(&events);
            let choice = &page["require_user_action"]["input"]["settings"][0]["field"]["dropdown"];
            assert_eq!(json!(HOST_NAME), choice["value"]);
            let device = choose_at(&controller, &events, name, HOST_NAME).await;
            assert_eq!(json!(HOST_NAME), device["address"]);
            assert_eq!(json!(HOST_NAME), device["host"]);
        }

        #[actix::test]
        async fn a_host_name_is_added_with_the_first_address_that_answers() {
            let receiver = crate::test_support::Fake::start(true).await;
            // Nothing answers at the first address.
            ports_by_address(1, receiver.port());
            resolve_to(&[FIRST_IP, SECOND_IP]);
            let name = "add-second-address";
            let controller = controller(name, None);
            let events = connect(&controller, "a").await;
            setup_fake_at(&controller, &events, false, HOST_NAME).await;
            let device = choose_at(&controller, &events, name, SECOND_IP).await;
            assert_eq!(json!(SECOND_IP), device["address"]);
            assert_eq!(json!(HOST_NAME), device["host"]);
        }

        /// A port of the loopback address that takes connections and never answers. Abort the
        /// task to close it.
        async fn silent_port() -> (u16, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind((FAKE_HOST, 0))
                .await
                .expect("bind");
            let port = listener.local_addr().expect("address").port();
            let task = tokio::spawn(async move {
                #[expect(
                    clippy::collection_is_never_read,
                    reason = "keeps the connections open"
                )]
                let mut open = Vec::new();
                while let Ok((socket, _)) = listener.accept().await {
                    open.push(socket);
                }
            });
            (port, task)
        }

        #[actix::test]
        async fn the_addresses_of_a_host_name_get_one_time_limit() {
            let (port, task) = silent_port().await;
            ports_by_address(port, port);
            resolve_to(&[FIRST_IP, SECOND_IP]);
            // The identify of each address takes at least the request time of the tests (500 ms).
            let limit = Duration::from_millis(200);
            let Err(error) = identify_entered(HOST_NAME.into(), limit).await else {
                panic!("no error");
            };
            assert!(error.is_timeout(), "{error}");
            // The error of the limit, not of an address.
            assert_eq!(
                Some(format!("identify {HOST_NAME}").as_str()),
                error.request()
            );

            // The first address fails at once and the second one has no answer: the error of
            // the first address.
            ports_by_address(1, port);
            let Err(error) = identify_entered(HOST_NAME.into(), limit).await else {
                panic!("no error");
            };
            assert_ne!(
                Some(format!("identify {HOST_NAME}").as_str()),
                error.request(),
                "{error}"
            );
            ports_by_address(port, port);

            // Three addresses: a limit for each address would take at least 1.5 s.
            resolve_to(&[FIRST_IP, SECOND_IP, "192.0.2.3"]);
            let ids = ReceiverIds {
                serial: None,
                mac: None,
            };
            let start = std::time::Instant::now();
            let limit = Duration::from_millis(600);
            let entered = Entered::resolve(HOST_NAME.into(), ids, limit).await;
            // The same as when no address answers: the first one.
            assert_eq!(
                Entered {
                    address: FIRST_IP.into(),
                    host: Some(HOST_NAME.into()),
                },
                entered
            );
            let elapsed = start.elapsed();
            assert!(elapsed < Duration::from_millis(1200), "{elapsed:?}");
            task.abort();
        }
    }
}
