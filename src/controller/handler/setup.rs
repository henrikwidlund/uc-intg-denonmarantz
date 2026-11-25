// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Driver setup flow: discovery, options and reconfiguration.

use crate::config_store::{
    DEFAULT_SIGNAL_SECS, DeviceConfig, DriverSettings, Feature, MIN_SIGNAL_SECS, SaveWait,
    backup_json, parse_backup,
};
use crate::controller::devices::ConnectTrigger;
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
use std::time::Duration;
use uc_api::intg::{DriverSetupChange, IntegrationSetup, SetupDriver};
use uc_api::model::intg::{IntegrationSetupError, IntegrationSetupState, SetupChangeEventType};
use uc_api::ws::{EventCategory, WsMessage};

/// Time between the acknowledgment of a setup request and the next setup event, for the web
/// configurator. The order does not depend on it: the acknowledgment goes first through the same
/// queue.
const ACK_DELAY: Duration = Duration::from_millis(100);
/// Highest time between signal reads in seconds.
const MAX_SIGNAL_POLL_SECS: u16 = 3600;

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
    host: String,
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
            host: receiver.host.clone(),
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
    fn from_identified(host: &str, id: &discovery::Identified, has_tuner: Option<bool>) -> Self {
        Self {
            host: host.to_owned(),
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
            ctx.run_later(ACK_DELAY, move |act, ctx| {
                if act.is_setup_flow(flow, &ws_id) {
                    act.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
                }
            });
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
            Err(InputError::Failed(error)) => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.finish_setup(&ws_id, Some(error), ctx);
                    }
                });
            }
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
            SetupState::Backup => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.finish_setup(&ws_id, None, ctx);
                    }
                });
            }
            SetupState::Restore => self.restore(ws_id, values, ctx)?,
            SetupState::ConfigMode => self.config_mode(ws_id, values, ctx)?,
            SetupState::ChooseDevice { mut found } => {
                let choice = values.get("choice").map_or_default(String::as_str);
                let Some(i) = found.iter().position(|f| f.host == choice) else {
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
                let options = match self
                    .devices
                    .get(key.as_str())
                    .map(|d| Options::of(&d.cfg))
                    .map(|o| o.read(values))
                {
                    Some(Ok(o)) => o,
                    Some(Err(e)) => {
                        return Err(InputError::Invalid(
                            Box::new(SetupState::Configure { key }),
                            e,
                        ));
                    }
                    None => return Err(InputError::Failed(IntegrationSetupError::NotFound)),
                };
                let save = self.configure(&key, address, options);
                self.finish_after_save(ws_id, save, ctx);
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
        let save = self.restore_devices(devices, ctx).map_err(|e| {
            warn!("[{ws_id}] saving the configuration failed: {e}");
            IntegrationSetupError::Other
        });
        self.finish_after_save(ws_id, save, ctx);
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
        let host = found.host.clone();
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
                    // The fields that the driver does not use keep their values.
                    let old =
                        act.find_device(identified.serial.as_deref(), identified.mac.as_deref());
                    let cfg = DeviceConfig {
                        update_audyssey: old.is_some_and(|c| c.update_audyssey),
                        zone2: old.is_some_and(|c| c.zone2),
                        zone3: old.is_some_and(|c| c.zone3),
                        timeout: old.map_or(2000, |c| c.timeout),
                        id: old.map_or(identified.id, |c| c.id.clone()),
                        id_kind: old.map_or(identified.id_kind, |c| c.id_kind),
                        serial: identified.serial,
                        mac: identified.mac,
                        name: identified.name,
                        address: found.host,
                        support_sound_mode: identified.support_sound_mode,
                        show_all_inputs: options.show_all_inputs,
                        use_telnet: options.use_telnet,
                        volume_step: old.map_or(0.5, |c| c.volume_step),
                        is_denon: options.is_denon,
                        is_dirac_supported: Some(identified.is_dirac_supported),
                        signal_poll_secs: Some(options.signal_poll_secs),
                        receiver_type: identified.kind.receiver_type,
                        update_method: identified.kind.update_method,
                        comm_api_version: identified.kind.comm_api_version,
                        has_tuner: identified.kind.has_tuner,
                        tuner: Some(options.has_tuner),
                        tuner_presets_lettered: old.and_then(|c| c.tuner_presets_lettered),
                        tuner_preset_names: old
                            .map_or_else(Vec::new, |c| c.tuner_preset_names.clone()),
                        network_favorite_names: old
                            .map_or_else(Vec::new, |c| c.network_favorite_names.clone()),
                        info: Some(identified.info),
                        // A new setup asks all sources again.
                        missing: denon_marantz::MissingSources::default(),
                    };
                    if act.setup_replace {
                        act.remove_other_devices(cfg.key());
                    }
                    if let Some(device) = act.devices.get_mut(cfg.key()) {
                        device.cfg = cfg;
                        device.send_config();
                        act.purge_entities();
                    } else {
                        act.add_device(cfg, ctx);
                    }
                    let save = act.save_devices().map_err(|e| {
                        warn!("[{ws_id}] saving the configuration failed: {e}");
                        IntegrationSetupError::Other
                    });
                    act.finish_after_save(ws_id, save, ctx);
                }),
        );
    }

    fn config_mode(
        &mut self,
        ws_id: String,
        values: &HashMap<String, String>,
        ctx: &mut Context<Self>,
    ) -> Result<(), InputError> {
        let unknown_device =
            || InputError::Invalid(Box::new(SetupState::ConfigMode), "Unknown device".into());
        let flow = self.setup_flow;
        let action = values.get("action").map_or_default(String::as_str);
        let key = values.get("choice").map_or_default(String::as_str);
        self.set_debug_log(
            &ws_id,
            parse_bool(values, "debug_log", self.driver_settings.debug_log),
        );
        match action {
            "settings" => self.finish_setup_later(ws_id, ctx),
            "add" => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
            "remove" => {
                let cfg = self.remove_device(key).ok_or_else(unknown_device)?;
                info!("[{ws_id}] removed {}", cfg.name);
                let save = self.save_devices().map_err(|e| {
                    warn!("[{ws_id}] saving the configuration failed: {e}");
                    IntegrationSetupError::Other
                });
                self.finish_after_save(ws_id, save, ctx);
            }
            "configure" => {
                let cfg = self
                    .devices
                    .get(key)
                    .ok_or_else(unknown_device)?
                    .cfg
                    .clone();
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_configure(ws_id, cfg, ctx);
                    }
                });
            }
            "backup" => {
                let text = backup_json(&self.sorted_configs())
                    .filter(|_| !self.devices.is_empty())
                    .ok_or_else(|| {
                        InputError::Invalid(
                            Box::new(SetupState::ConfigMode),
                            "Nothing to back up".into(),
                        )
                    })?;
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.setup = Some(SetupState::Backup);
                        act.request_input(&ws_id, backup_page(&text), ctx);
                    }
                });
            }
            "restore" => {
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.setup = Some(SetupState::Restore);
                        act.request_input(&ws_id, restore_page(), ctx);
                    }
                });
            }
            "reset" => {
                // The receivers are replaced when the setup ends with a receiver.
                self.setup_replace = true;
                info!("[{ws_id}] configuration reset after the setup");
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.request_discover(&ws_id, ctx);
                    }
                });
            }
            _ => {
                return Err(InputError::Invalid(
                    Box::new(SetupState::ConfigMode),
                    format!("Unknown action: {action}"),
                ));
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

    /// End the setup when the configuration is written, without blocking the thread. A write
    /// error ends the setup with an error.
    fn finish_after_save(
        &self,
        ws_id: String,
        save: Result<SaveWait, IntegrationSetupError>,
        ctx: &mut Context<Self>,
    ) {
        let flow = self.setup_flow;
        ctx.spawn(
            async move {
                match save {
                    Ok(wait) => wait.wait().await.map_err(|e| {
                        warn!("Saving the configuration failed: {e}");
                        IntegrationSetupError::Other
                    }),
                    Err(e) => Err(e),
                }
            }
            .into_actor(self)
            .map(move |result, _, ctx| {
                // The request got its answer first.
                ctx.run_later(ACK_DELAY, move |act, ctx| {
                    if act.is_setup_flow(flow, &ws_id) {
                        act.finish_setup(&ws_id, result.err(), ctx);
                    }
                });
            }),
        );
    }

    /// End the setup after the request was acknowledged.
    fn finish_setup_later(&self, ws_id: String, ctx: &mut Context<Self>) {
        let flow = self.setup_flow;
        ctx.run_later(ACK_DELAY, move |act, ctx| {
            if act.is_setup_flow(flow, &ws_id) {
                act.finish_setup(&ws_id, None, ctx);
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

    /// Remove all devices but the one with `key`, and the cached states of their entities. The
    /// caller saves.
    fn remove_other_devices(&mut self, key: &str) {
        let keys: Vec<String> = self
            .devices
            .keys()
            .filter(|k| ***k != *key)
            .map(ToString::to_string)
            .collect();
        for key in &keys {
            self.remove_device(key);
        }
    }

    /// Replace all devices with the devices of a backup.
    fn restore_devices(
        &mut self,
        devices: Vec<DeviceConfig>,
        ctx: &Context<Self>,
    ) -> Result<SaveWait, ServiceError> {
        let keys: Vec<String> = self.devices.keys().map(ToString::to_string).collect();
        for key in &keys {
            self.remove_device(key);
        }
        for cfg in devices {
            self.add_device(cfg, ctx);
        }
        // Forget the cached states of entities that are gone.
        self.purge_entities();
        self.save_devices()
    }

    /// Apply the options page of a configured device.
    fn configure(
        &mut self,
        key: &str,
        address: Option<String>,
        options: Options,
    ) -> Result<SaveWait, IntegrationSetupError> {
        let device = self
            .devices
            .get_mut(key)
            .ok_or(IntegrationSetupError::NotFound)?;
        let cfg = &mut device.cfg;
        cfg.show_all_inputs = options.show_all_inputs;
        cfg.is_denon = options.is_denon;
        cfg.use_telnet = options.use_telnet;
        cfg.is_dirac_supported = Some(options.is_dirac_supported);
        cfg.tuner = Some(options.has_tuner);
        cfg.signal_poll_secs = Some(options.signal_poll_secs);
        if let Some(address) = address {
            cfg.address = address;
        }
        device.send_config();
        // Entities can come or go, for example when the connection mode changes.
        self.purge_entities();
        self.save_devices().map_err(|e| {
            warn!("Saving the configuration failed: {e}");
            IntegrationSetupError::Other
        })
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
            Some(SetupState::Backup) => backup_json(&self.sorted_configs())
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
        let mut devices: Vec<_> = self.devices.values().map(|d| &d.cfg).collect();
        devices.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let mut items: Vec<Value> = devices
            .iter()
            .map(|c| json!({ "id": c.id, "label": { "en": format!("{} ({} - {})", c.name, c.id, c.address) } }))
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
    format!("{} ({} - {})", f.name, f.model, f.host)
}

fn choose_device_page(found: &[Found]) -> Value {
    let items: Vec<Value> = found
        .iter()
        .map(|f| json!({ "id": f.host, "label": { "en": device_label(f) } }))
        .collect();
    let first = &found[0];
    let mut settings = vec![json!({
        "id": "choice",
        "label": { "en": "Choose your Denon or Marantz AVR" },
        "field": { "dropdown": { "value": first.host, "items": items } }
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
        "field": { "text": { "value": cfg.address } }
    })];
    settings.extend(Options::of(cfg).fields());
    json!({
        "title": { "en": "Configure your AVR" },
        "settings": settings
    })
}

/// The receivers at one address, or all that answer the search.
async fn find_receivers(address: Option<String>) -> Result<Vec<Found>, AvrError> {
    if let Some(host) = address {
        let avr = discovery::identify(&host, None).await?;
        let id = discovery::Identified::new(&avr)
            .map_err(|reason| AvrError::InvalidRequest(format!("[{host}] not used: {reason}")))?;
        let tuner = detect_tuner(&avr).await;
        return Ok(vec![Found::from_identified(&host, &id, tuner)]);
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
                host: "h".into(),
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
            host: "h".into(),
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
        use crate::config_store::backup_json;
        use crate::controller::handler::setup::{SetDriverUserDataMsg, SetupDriverMsg};
        use crate::controller::test_util::{
            DeviceAddr, Subscribed, SubscribesAll, TestRemote, config_file, connect, controller,
            settle, use_closed_port, wait_stopped,
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
            let devices: Vec<crate::config_store::DeviceConfig> =
                serde_json::from_str(PYTHON_CONFIG).expect("devices");
            let backup = backup_json(&devices).expect("backup");
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
                .send(values("a", &[("address", FAKE_HOST)]))
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
            assert!(
                entity_ids(&controller)
                    .await
                    .iter()
                    .any(|id| id == "media_player.0123456789AB")
            );
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
    }
}
