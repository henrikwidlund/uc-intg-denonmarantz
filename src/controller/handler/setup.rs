// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Driver setup flow: discovery, options and reconfiguration.

use crate::config_store::{DeviceConfig, DriverSettings, SaveWait, backup_json, parse_backup};
use crate::controller::handler::AbortDriverSetup;
use crate::controller::{Controller, operation_mode};
use crate::device::{UpdateConfig, discovery};
use crate::entities::EntityKind;
use crate::errors::ServiceError;
use crate::logging;
use actix::{ActorFutureExt, AsyncContext, Context, Handler, Message, WrapFuture};
use denon_marantz::Error as AvrError;
use log::{debug, info, warn};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use uc_api::intg::{DriverSetupChange, IntegrationSetup, SetupDriver};
use uc_api::model::intg::{IntegrationSetupError, IntegrationSetupState, SetupChangeEventType};
use uc_api::ws::{EventCategory, WsMessage};

/// Delay so that the request is acknowledged before the next setup event.
const ACK_DELAY: Duration = Duration::from_millis(100);
/// Lowest and highest volume step in dB.
const MIN_VOLUME_STEP: f32 = 0.1;
const MAX_VOLUME_STEP: f32 = 10.0;
/// Highest time between signal reads in seconds.
const MAX_SIGNAL_POLL_SECS: u16 = 3600;

/// Start the setup flow.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetupDriverMsg {
    pub ws_id: String,
    pub data: SetupDriver,
}

/// User input of a setup page.
#[derive(Debug, Message)]
#[rtype(result = "Result<(), ServiceError>")]
pub struct SetDriverUserDataMsg {
    pub ws_id: String,
    pub data: IntegrationSetup,
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
}

impl Found {
    fn new(receiver: &denon_marantz::DiscoveredReceiver, is_dirac_supported: bool) -> Self {
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
            is_dirac_supported,
        }
    }

    /// A receiver that the user gave the address of.
    fn from_identified(host: &str, id: &discovery::Identified) -> Self {
        Self {
            host: host.to_owned(),
            name: id.name.clone(),
            model: id.model.clone(),
            serial: id.serial.clone(),
            mac: id.mac.clone(),
            is_denon: id.is_denon,
            is_dirac_supported: id.is_dirac_supported,
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
    volume_step: f32,
    is_dirac_supported: bool,
    signal_poll_secs: u16,
}

impl Options {
    const fn of(cfg: &DeviceConfig) -> Self {
        Self {
            show_all_inputs: cfg.show_all_inputs,
            is_denon: cfg.is_denon,
            use_telnet: cfg.use_telnet,
            volume_step: cfg.volume_step,
            is_dirac_supported: cfg.has_dirac(),
            signal_poll_secs: cfg.signal_poll_secs,
        }
    }

    const fn for_found(found: &Found) -> Self {
        Self {
            show_all_inputs: false,
            is_denon: found.is_denon,
            use_telnet: true,
            volume_step: 1.0,
            is_dirac_supported: found.is_dirac_supported,
            signal_poll_secs: 0,
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
        if let Some(step) = values.get("volume_step") {
            let step: f32 = step
                .trim()
                .replace(',', ".")
                .parse()
                .map_err(|_| format!("Invalid volume step: {step}"))?;
            if !(MIN_VOLUME_STEP..=MAX_VOLUME_STEP).contains(&step) {
                return Err(format!(
                    "The volume step must be from {MIN_VOLUME_STEP} to {MAX_VOLUME_STEP}"
                ));
            }
            self.volume_step = step;
        }
        if let Some(v) = values.get("is_dirac_supported") {
            self.is_dirac_supported = v.eq_ignore_ascii_case("true");
        }
        if let Some(secs) = values.get("signal_poll_secs") {
            let secs = secs.trim();
            self.signal_poll_secs = if secs.is_empty() {
                0
            } else {
                secs.parse()
                    .ok()
                    .filter(|s| *s <= MAX_SIGNAL_POLL_SECS)
                    .ok_or_else(|| {
                        format!("The signal read time must be from 0 to {MAX_SIGNAL_POLL_SECS}")
                    })?
            };
        }
        Ok(self)
    }

    /// The fields of the page, with the labels of the Python integration.
    fn fields(&self) -> Vec<Value> {
        vec![
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
                "id": "volume_step",
                "label": { "en": "Volume step" },
                "field": { "number": {
                    "value": self.volume_step, "min": 0.5, "max": 10, "steps": 1,
                    "decimals": 1, "unit": { "en": "dB" }
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
                "id": "signal_poll_secs",
                "label": { "en": "Read the signal info every N seconds (0: only when the receiver reports it)" },
                "field": { "number": {
                    "value": self.signal_poll_secs, "min": 0, "max": MAX_SIGNAL_POLL_SECS,
                    "steps": 1, "decimals": 0, "unit": { "en": "s" }
                } }
            }),
            json!({
                "id": "info",
                "label": { "en": "Please note:" },
                "field": { "label": { "value": { "en":
                    "Using telnet provides realtime updates for many values but certain receivers allow a single connection only!"
                } } }
            }),
        ]
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
            // A first setup starts with a clean configuration.
            if let Err(e) = self.reset_devices() {
                self.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
                return Err(e);
            }
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
        let Some(step) = self.setup.take() else {
            self.finish_setup(&ws_id, Some(IntegrationSetupError::Other), ctx);
            return Err(ServiceError::BadRequest("No active setup step".into()));
        };
        let flow = self.setup_flow;
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
        // A reconfiguration that was aborted keeps the configured devices.
        if let Some(session) = self.sessions.get_mut(ws_id) {
            session.reconfiguring = None;
        }
        if !self.devices.is_empty() {
            let _ = self.sm_consume(ws_id, &operation_mode::Input::ConfigurationAvailable, ctx);
            self.connect_devices();
        }
    }

    /// Forget the active setup flow. Tasks of the old flow are ignored.
    fn end_setup_flow(&mut self) {
        self.setup = None;
        self.setup_owner = None;
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
                    let found: Vec<Found> = found
                        .into_iter()
                        .filter(|f| {
                            act.find_device(f.serial.as_deref(), f.mac.as_deref())
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
                        volume_step: options.volume_step,
                        is_denon: options.is_denon,
                        is_dirac_supported: Some(identified.is_dirac_supported),
                        signal_poll_secs: options.signal_poll_secs,
                        receiver_type: identified.kind.receiver_type,
                        update_method: identified.kind.update_method,
                        hdmi_output_over_http: identified.kind.hdmi_output_over_http,
                        quick_select_count: identified.kind.quick_select_count,
                        listed_queries: identified.kind.listed_queries,
                    };
                    if let Some(device) = act.devices.get_mut(cfg.key()) {
                        device.cfg = cfg.clone();
                        device.addr.do_send(UpdateConfig(cfg));
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
                for session in self.sessions.values_mut() {
                    session
                        .subscribed_entities
                        .retain(|id| EntityKind::parse(id).is_none_or(|(k, _)| k != key));
                }
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
                if let Err(e) = self.reset_devices() {
                    warn!("[{ws_id}] saving the configuration failed: {e}");
                    return Err(InputError::Failed(IntegrationSetupError::Other));
                }
                info!("[{ws_id}] configuration reset");
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
    /// if it has Dirac. If it does not answer, the saved value is shown.
    fn request_configure(&self, ws_id: String, cfg: DeviceConfig, ctx: &mut Context<Self>) {
        let flow = self.setup_flow;
        let host = cfg.address.clone();
        ctx.spawn(
            async move { detect_dirac_opt(&host).await }
                .into_actor(self)
                .map(move |detected, act, ctx| {
                    if !act.is_setup_flow(flow, &ws_id) {
                        return;
                    }
                    let mut cfg = cfg;
                    if let Some(dirac) = detected {
                        cfg.is_dirac_supported = Some(dirac);
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

    /// Remove all devices and their entities.
    fn reset_devices(&mut self) -> Result<(), ServiceError> {
        if self.devices.is_empty() {
            return Ok(());
        }
        let keys: Vec<String> = self.devices.keys().map(ToString::to_string).collect();
        for key in &keys {
            self.remove_device(key);
        }
        self.entities.clear();
        for session in self.sessions.values_mut() {
            session.subscribed_entities.clear();
        }
        // The setup goes on at once: a write error is logged.
        self.queue_devices()
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
        // Receivers that are in the backup too keep their subscriptions.
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
        cfg.volume_step = options.volume_step;
        cfg.is_dirac_supported = Some(options.is_dirac_supported);
        cfg.signal_poll_secs = options.signal_poll_secs;
        if let Some(address) = address {
            cfg.address = address;
        }
        device.addr.do_send(UpdateConfig(cfg.clone()));
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
            self.connect_devices();
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
        return Ok(vec![Found::from_identified(&host, &id)]);
    }
    let receivers = discovery::discover().await?;
    // The search does not say if the receiver has Dirac: ask each one.
    let dirac = futures::future::join_all(receivers.iter().map(|r| detect_dirac(&r.host))).await;
    Ok(receivers
        .iter()
        .zip(dirac)
        .map(|(r, d)| Found::new(r, d))
        .collect())
}

/// Time to wait for the answer of a receiver to the Dirac question.
const DETECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask the receiver if it supports Dirac Live. `None` if it does not answer.
async fn detect_dirac(host: &str) -> bool {
    detect_dirac_opt(host).await.unwrap_or(false)
}

async fn detect_dirac_opt(host: &str) -> Option<bool> {
    let avr = tokio::time::timeout(DETECT_TIMEOUT, discovery::identify(host, None))
        .await
        .ok()?
        .ok()?;
    Some(avr.info().dirac_live)
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

/// Address of a setup page. An empty text is no address.
fn parse_address(value: Option<&str>) -> Result<Option<String>, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) if v.contains(char::is_whitespace) => Err(format!("Invalid address: {v}")),
        Some(v) => Ok(Some(v.to_owned())),
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
    }

    fn options() -> Options {
        Options {
            show_all_inputs: false,
            is_denon: true,
            use_telnet: true,
            volume_step: 0.5,
            is_dirac_supported: false,
            signal_poll_secs: 0,
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
                ("volume_step", "1,5"),
                ("is_dirac_supported", "True"),
                ("signal_poll_secs", "10"),
            ]))
            .expect("ok");
        assert!(!o.is_denon && o.show_all_inputs && !o.use_telnet && o.is_dirac_supported);
        assert!((o.volume_step - 1.5).abs() < f32::EPSILON);
        assert_eq!(10, o.signal_poll_secs);
        // Missing fields keep the values.
        assert_eq!(options(), options().read(&HashMap::new()).expect("ok"));
    }

    #[test]
    fn invalid_options_are_rejected() {
        for (key, value) in [
            ("volume_step", "0"),
            ("volume_step", "11"),
            ("volume_step", "abc"),
            ("signal_poll_secs", "-1"),
            ("signal_poll_secs", "99999"),
        ] {
            assert!(
                options().read(&values(&[(key, value)])).is_err(),
                "{key}={value}"
            );
        }
    }

    mod flow {
        use crate::config_store::{DriverSettings, backup_json};
        use crate::configuration::{HeartbeatSettings, Settings};
        use crate::controller::handler::setup::{SetDriverUserDataMsg, SetupDriverMsg};
        use crate::controller::{
            Controller, NewR2Session, R2RequestMsg, R2SessionDisconnect, SendWsMessage,
        };
        use actix::{Actor, Addr, Context, Handler};
        use serde_json::{Value, json};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use uc_api::intg::IntegrationSetup;
        use uc_api::intg::ws::R2Request;

        /// Collects the events sent to a Remote.
        struct Remote(Arc<Mutex<Vec<Value>>>);

        impl Actor for Remote {
            type Context = Context<Self>;
        }

        impl Handler<SendWsMessage> for Remote {
            type Result = ();
            fn handle(&mut self, msg: SendWsMessage, _: &mut Context<Self>) {
                let SendWsMessage::Msg(msg) = msg else { return };
                if let (Some("driver_setup_change"), Some(data)) =
                    (msg.msg.as_deref(), msg.msg_data)
                    && let Ok(mut events) = self.0.lock()
                {
                    events.push(data);
                }
            }
        }

        /// A controller with a new configuration folder. `config` is written to `config.json`.
        fn controller(name: &str, config: Option<&str>) -> Addr<Controller> {
            let dir = std::env::temp_dir().join(format!(
                "uc-intg-denonmarantz-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            if let Some(config) = config {
                std::fs::write(dir.join("config.json"), config).expect("config");
            }
            let settings = Settings {
                interface: "127.0.0.1".into(),
                http_port: 0,
                config_home: dir,
                heartbeat: HeartbeatSettings::default(),
            };
            let metadata = serde_json::from_value(json!({})).expect("metadata");
            Controller::new(settings, DriverSettings::default(), metadata).start()
        }

        async fn connect(controller: &Addr<Controller>, id: &str) -> Arc<Mutex<Vec<Value>>> {
            let events = Arc::default();
            let addr = Remote(Arc::clone(&events)).start();
            controller
                .send(NewR2Session {
                    addr: addr.recipient(),
                    id: id.into(),
                })
                .await
                .expect("send");
            events
        }

        fn values(ws_id: &str, values: &[(&str, &str)]) -> SetDriverUserDataMsg {
            let values = values
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect();
            SetDriverUserDataMsg {
                ws_id: ws_id.into(),
                data: IntegrationSetup::InputValues(values),
            }
        }

        fn start_setup(ws_id: &str, reconfigure: bool) -> SetupDriverMsg {
            let data =
                serde_json::from_value(json!({ "reconfigure": reconfigure, "setup_data": {} }))
                    .expect("setup");
            SetupDriverMsg {
                ws_id: ws_id.into(),
                data,
            }
        }

        fn last(events: &Mutex<Vec<Value>>) -> Value {
            events
                .lock()
                .expect("lock")
                .last()
                .cloned()
                .unwrap_or_default()
        }

        async fn settle() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        /// Wait until the last event has this setup state. The configuration is saved off the
        /// actix thread, so the end of a setup can take longer than [`settle`] on a slow machine.
        async fn wait_state(events: &Mutex<Vec<Value>>, state: &str) {
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

        #[actix::test]
        async fn a_python_configuration_gives_the_python_entities() {
            let controller = controller("python-config", Some(PYTHON_CONFIG));
            connect(&controller, "a").await;
            let ids = entity_ids(&controller).await;
            // The 42 entities of the Python integration (media player, remote, 11 selects and 29
            // sensors), and 13 more: 4 selects, 6 switches, 2 numbers and 1 sensor.
            assert_eq!(55, ids.len());
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
            settle().await;
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
            settle().await;
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
            settle().await;
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
            settle().await;

            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            settle().await;
            assert_eq!(
                json!("restore_data"),
                last(&events)["require_user_action"]["input"]["settings"][1]["id"]
            );

            controller
                .send(values("a", &[("restore_data", "secret-not-a-backup")]))
                .await
                .expect("send")
                .expect("ok");
            settle().await;
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
            settle().await;
            controller
                .send(values("a", &[("restore_from_backup", "true")]))
                .await
                .expect("send")
                .expect("ok");
            settle().await;
            controller
                .send(values("a", &[("restore_data", &backup)]))
                .await
                .expect("send")
                .expect("ok");
            wait_state(&events, "OK").await;
            assert_eq!(55, entity_ids(&controller).await.len());
        }

        /// The states that a Remote gets when it asks for them.
        async fn entity_states(controller: &Addr<Controller>) -> Vec<String> {
            let msg = R2RequestMsg {
                ws_id: "a".into(),
                req_id: 2,
                request: R2Request::GetEntityStates,
                msg_data: None,
            };
            let response = controller
                .send(msg)
                .await
                .expect("send")
                .expect("ok")
                .expect("response");
            response
                .msg_data
                .expect("data")
                .as_array()
                .expect("list")
                .iter()
                .filter_map(|e| e["entity_id"].as_str().map(str::to_owned))
                .collect()
        }

        #[actix::test]
        async fn entities_that_go_away_are_forgotten() {
            let controller = controller("purge", Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            settle().await;
            assert_eq!(55, entity_states(&controller).await.len());
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            settle().await;
            controller
                .send(values(
                    "a",
                    &[("choice", "SERIAL0000001"), ("action", "configure")],
                ))
                .await
                .expect("send")
                .expect("ok");
            // The page comes after the check for Dirac, which waits for an address that does not
            // answer.
            tokio::time::sleep(Duration::from_millis(2500)).await;
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
            assert_eq!(15, entity_states(&controller).await.len());
        }

        /// Collects the `entity_change` events sent to a Remote.
        struct Changes(Arc<Mutex<Vec<String>>>);

        impl Actor for Changes {
            type Context = Context<Self>;
        }

        impl Handler<SendWsMessage> for Changes {
            type Result = ();
            fn handle(&mut self, msg: SendWsMessage, _: &mut Context<Self>) {
                if let SendWsMessage::Text(text) = msg
                    && text.contains(r#""msg":"entity_change""#)
                    && let Ok(mut changes) = self.0.lock()
                {
                    changes.push(text.to_string());
                }
            }
        }

        async fn connect_changes(
            controller: &Addr<Controller>,
            id: &str,
        ) -> Arc<Mutex<Vec<String>>> {
            let changes = Arc::default();
            let addr = Changes(Arc::clone(&changes)).start();
            controller
                .send(NewR2Session {
                    addr: addr.recipient(),
                    id: id.into(),
                })
                .await
                .expect("send");
            changes
        }

        fn count(changes: &Mutex<Vec<String>>, part: &str) -> usize {
            changes
                .lock()
                .expect("lock")
                .iter()
                .filter(|c| c.contains(part))
                .count()
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
            let controller = controller("subscribe", Some(PYTHON_CONFIG));
            let changes = connect_changes(&controller, "a").await;
            settle().await;
            let (rest, first): (Vec<String>, Vec<String>) = entity_ids(&controller)
                .await
                .into_iter()
                .partition(|id| id.starts_with("media_player."));
            // The start of a session: one event per entity can fill the event queue of the Remote.
            subscribe(&controller, "a", &first).await;
            assert_eq!(0, count(&changes, ""));
            assert_eq!(first.len(), entity_states(&controller).await.len());
            // An entity that is added later gets its state at once.
            subscribe(&controller, "a", &rest).await;
            assert_eq!(1, count(&changes, ""));
        }

        #[actix::test]
        async fn a_receiver_can_be_removed() {
            let controller = controller("remove", Some(PYTHON_CONFIG));
            let events = connect(&controller, "a").await;
            controller
                .send(start_setup("a", true))
                .await
                .expect("send")
                .expect("setup started");
            settle().await;
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
        }

        /// The address of the fake receiver in the setup.
        const FAKE_HOST: &str = "127.0.0.1";

        /// A fake receiver at [`FAKE_HOST`]. Its telnet port does not answer.
        async fn fake_receiver() -> crate::test_support::Fake {
            fake_receiver_from(crate::test_support::Fake::start(true).await)
        }

        fn fake_receiver_from(http: crate::test_support::Fake) -> crate::test_support::Fake {
            let port = http.port();
            let setup: crate::device::discovery::TestBuilder =
                std::rc::Rc::new(move |_, builder| {
                    crate::test_support::test_builder(port, 1, builder)
                });
            crate::device::discovery::TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
            http
        }

        /// Start the setup and give the address of the fake receiver. `reconfigure` keeps the
        /// configured receivers.
        async fn setup_fake(
            controller: &Addr<Controller>,
            events: &Mutex<Vec<Value>>,
            reconfigure: bool,
        ) {
            controller
                .send(start_setup("a", reconfigure))
                .await
                .expect("send")
                .expect("setup started");
            settle().await;
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
            settle().await;
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

        fn config_file(name: &str) -> Value {
            let dir = std::env::temp_dir().join(format!(
                "uc-intg-denonmarantz-{name}-{}",
                std::process::id()
            ));
            let text = std::fs::read_to_string(dir.join("config.json")).expect("config.json");
            serde_json::from_str(&text).expect("json")
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
