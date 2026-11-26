// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Configured receivers and per-device cache files.
//!
//! Files are written to a temporary file first and then renamed, so a crash never leaves a broken
//! file. The configuration file is only readable by the owner.
//!
//! The data is serialized on the caller thread. One background thread writes the files in order,
//! so the actor thread does not wait for the disk.

use denon_marantz::{Brand, HttpReads, MissingSources, Query, SpecFeature, SpecGeneration};
use log::{error, info};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

const CONFIG_FILE: &str = "config.json";
const SETTINGS_FILE: &str = "settings.json";

/// The `Avr` methods that the HTTP mode calls on its timers: `update`, and
/// `refresh_signal_pages` (it also does the read of `update_signal_and_settings`). With a signal
/// time of 0, `update_signal_and_settings` comes in place of `refresh_signal_pages`.
const POLL_READS: HttpReads = HttpReads::UPDATE
    .union(HttpReads::SIGNAL_AND_SETTINGS)
    .union(HttpReads::SIGNAL_PAGES);

/// `POLL_READS` with a signal time of 0.
const POLL_READS_NO_PAGES: HttpReads = HttpReads::UPDATE.union(HttpReads::SIGNAL_AND_SETTINGS);

/// Signal time without a value in the configuration.
pub const DEFAULT_SIGNAL_SECS: u16 = 5;
/// Lowest signal time above 0: the library check must be above its answer wait of 2 s.
pub const MIN_SIGNAL_SECS: u16 = 3;

/// What an entity gets in HTTP mode before the receiver is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeforeRead {
    /// The entity comes: the Python integration has it without telnet.
    Shown,
    /// The entity does not come.
    Hidden,
}

/// One configured receiver.
///
/// The field names are the ones of the Python integration, so its `config.json` loads without
/// changes. Unknown fields of the file are ignored.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// MAC address or serial number of the receiver, see `id_kind`. It is also the device key of
    /// the entity ids.
    pub id: String,
    /// What `id` is. Missing in older files: then it is the serial number. It is not written for
    /// the serial number, so a file of the Python integration stays the same.
    #[serde(
        default,
        skip_serializing_if = "IdKind::is_serial",
        deserialize_with = "serial_if_unknown"
    )]
    pub id_kind: IdKind,
    /// Serial number of the receiver, if known.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_empty"
    )]
    pub serial: Option<String>,
    /// MAC address of the receiver as 12 uppercase hexadecimal digits, if known.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "mac_if_valid"
    )]
    pub mac: Option<String>,
    pub name: String,
    /// The address that the driver connects to: an IP address, or a host name (not resolved in the
    /// setup, or from an old configuration).
    pub address: String,
    /// The host name from the setup, or an old `address` that was a host name. A connect resolves it
    /// when the receiver does not answer at `address`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default = "default_true", deserialize_with = "true_if_null")]
    pub support_sound_mode: bool,
    #[serde(default, deserialize_with = "false_if_null")]
    pub show_all_inputs: bool,
    /// Telnet and HTTP, or HTTP only.
    #[serde(default = "default_true", deserialize_with = "true_if_null")]
    pub use_telnet: bool,
    /// Not used. Kept for the Python integration.
    #[serde(default, deserialize_with = "false_if_null")]
    pub update_audyssey: bool,
    /// Not used. Kept for the Python integration.
    #[serde(default, deserialize_with = "false_if_null")]
    pub zone2: bool,
    /// Not used. Kept for the Python integration.
    #[serde(default, deserialize_with = "false_if_null")]
    pub zone3: bool,
    /// Not used: volume up and down use the step of the receiver. Kept for the Python integration.
    #[serde(default = "default_volume_step", deserialize_with = "step_if_null")]
    pub volume_step: f32,
    /// Not used. Kept for the Python integration.
    #[serde(default = "default_timeout", deserialize_with = "timeout_if_null")]
    pub timeout: u32,
    #[serde(default = "default_true", deserialize_with = "true_if_null")]
    pub is_denon: bool,
    #[serde(default)]
    pub is_dirac_supported: Option<bool>,
    /// Seconds between signal checks. `None`: `DEFAULT_SIGNAL_SECS`. Use `signal_secs` to read or
    /// compare it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_poll_secs: Option<u16>,
    /// The receiver generation. It is found at the setup and checked at each connect.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub receiver_type: Option<ReceiverGeneration>,
    /// How the receiver is read over HTTP. It is found at the setup and checked at each connect.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub update_method: Option<HttpReadMethod>,
    /// `CommApiVers` of `Deviceinfo.xml`: the protocol generation. It decides HEOS and the commands
    /// of older receivers. It is found at the setup and checked at each connect.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub comm_api_version: Option<CommApiVersion>,
    /// The receiver has a tuner. `None` if `Deviceinfo.xml` does not say. It is found at the setup
    /// and checked at each connect.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub has_tuner: Option<bool>,
    /// The setup option "Receiver has a tuner". It wins over `has_tuner`. `None` until the
    /// receiver is set up with this driver.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub tuner: Option<bool>,
    /// The tuner presets are `A1` to `G8` (`true`) or `01` to `56` (`false`). The receiver shows it
    /// in its preset answers. `None` until it did.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub tuner_presets_lettered: Option<bool>,
    /// The tuner presets as the select shows them: the name, or the frequency for a preset without
    /// a name. The first is preset 1. Kept, so the select shows them before the receiver is asked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tuner_preset_names: Vec<String>,
    /// The names of the network favorites. The first is favorite 1, empty if it has no name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_favorite_names: Vec<String>,
    /// What the library found at the last full connect. With it, the next connect sends no
    /// request. A value from another library version that does not load is `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub info: Option<denon_marantz::Info>,
    /// The HTTP sources that the receiver does not have: a web page that answered 404, or a value
    /// that `Deviceinfo.xml` does not list or that a read dropped. The entities leave out their
    /// values, and the next connect does not ask them. Cleared when `info` changes and at a new
    /// setup. A value that does not load is the default.
    #[serde(
        default,
        skip_serializing_if = "is_default",
        deserialize_with = "default_if_unknown"
    )]
    pub missing: MissingSources,
}

/// What the id of a receiver is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdKind {
    /// The serial number. Old receivers give only this.
    #[default]
    Serial,
    /// The MAC address.
    Mac,
}

impl IdKind {
    #[allow(clippy::trivially_copy_pass_by_ref)] // The signature that serde needs.
    const fn is_serial(&self) -> bool {
        matches!(self, Self::Serial)
    }
}

/// What the driver found on the receiver. A change makes the controller check the entities again.
/// Which entities come follows `DeviceConfig::info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReceiverKind {
    pub receiver_type: Option<ReceiverGeneration>,
    pub update_method: Option<HttpReadMethod>,
    pub comm_api_version: Option<CommApiVersion>,
    pub has_tuner: Option<bool>,
}

/// The receiver generation, as the library reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiverGeneration {
    Avr,
    AvrX,
    AvrX2016,
}

/// How the receiver is read over HTTP, as the library reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpReadMethod {
    AppCommand,
    StatusPages,
}

/// `CommApiVers` of `Deviceinfo.xml`, for example `0301`. It is written as these 4 digits, or as
/// `none` for a receiver that does not report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommApiVersion(u16);

impl CommApiVersion {
    /// The receiver does not report a version. Receivers of about 2012 and older have no
    /// `Deviceinfo.xml`.
    pub const MISSING: Self = Self(0);

    /// Reads the 4 digits, or `none`. `None` for other text.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text == "none" {
            return Some(Self::MISSING);
        }
        if text.len() != 4 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok().map(Self)
    }
}

/// What only some receivers have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// A command group that only the specs of some generations have.
    Spec(SpecFeature),
    /// The tuner: the setup option, else all but a receiver that reports no tuner.
    Tuner,
    /// `PSDIRAC`: Dirac Live filters, from the setup.
    Dirac,
}

impl Feature {
    /// The name in `resources/commands.tsv`. The library finds the other spec features from the
    /// command text.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "network_audio" => Self::Spec(SpecFeature::NetworkAudio),
            "tuner" => Self::Tuner,
            "dirac" => Self::Dirac,
            _ => return None,
        })
    }
}

impl Serialize for CommApiVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if *self == Self::MISSING {
            return s.serialize_str("none");
        }
        s.collect_str(&format_args!("{:04}", self.0))
    }
}

impl<'de> Deserialize<'de> for CommApiVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("not 4 digits"))
    }
}

impl DeviceConfig {
    /// Key for the entity ids.
    pub fn key(&self) -> &str {
        &self.id
    }

    /// The order of the receivers in lists and in the configuration file: by name, then by key.
    pub fn cmp_list_order(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| self.key().cmp(other.key()))
    }

    /// The serial number: the stored one, or the id if it is the serial number.
    pub fn serial(&self) -> Option<&str> {
        match self.id_kind {
            IdKind::Serial => Some(&self.id),
            IdKind::Mac => self.serial.as_deref(),
        }
    }

    /// The MAC address: the stored one, or the id if it is the MAC address.
    pub fn mac(&self) -> Option<&str> {
        match self.id_kind {
            IdKind::Mac => Some(&self.id),
            IdKind::Serial => self.mac.as_deref(),
        }
    }

    /// Whether a receiver with this serial number and MAC address is this one. The MAC address is
    /// compared if both are known, else the serial number. `None` if neither can be compared.
    pub fn same_receiver(&self, serial: Option<&str>, mac: Option<&str>) -> Option<bool> {
        if let (Some(a), Some(b)) = (self.mac().and_then(parse_mac), mac.and_then(parse_mac)) {
            return Some(a == b);
        }
        if let (Some(a), Some(b)) = (self.serial(), serial) {
            return Some(a == b);
        }
        None
    }

    /// The address for the user: the host name if there is one, else `address`.
    pub fn shown_address(&self) -> &str {
        self.host.as_deref().unwrap_or(&self.address)
    }

    /// Take a new address that a connect found. An old address that is not an IP address is
    /// kept as `host` if no host name is stored. An empty old address is not kept, and neither is
    /// an IPv6 address in brackets.
    pub fn set_address(&mut self, address: String) {
        let old = std::mem::replace(&mut self.address, address);
        let bare = old
            .strip_prefix('[')
            .and_then(|a| a.strip_suffix(']'))
            .unwrap_or(&old);
        if self.host.is_none() && !bare.is_empty() && !is_ip(bare) {
            self.host = Some(old);
        }
    }

    /// Store the serial number and the MAC address if they are not stored yet. Returns true if one
    /// was added.
    pub fn add_ids(&mut self, serial: Option<&str>, mac: Option<&str>) -> bool {
        let before = (self.serial.is_some(), self.mac.is_some());
        if self.serial.is_none() {
            self.serial = serial.filter(|s| !s.is_empty()).map(str::to_owned);
        }
        if self.mac.is_none() {
            self.mac = mac.and_then(mac_text);
        }
        before != (self.serial.is_some(), self.mac.is_some())
    }

    pub const fn has_dirac(&self) -> bool {
        matches!(self.is_dirac_supported, Some(true))
    }

    /// The driver gets the value: telnet gives every value, and over HTTP a value comes if a
    /// repeated read gives it (see `Info::http_reads_without`, `Self::missing` and
    /// `POLL_READS`). Before the receiver is read, `before_read` decides.
    pub fn reads_value(&self, query: Query, before_read: BeforeRead) -> bool {
        self.use_telnet
            || self
                .info
                .as_ref()
                .map_or(before_read == BeforeRead::Shown, |info| {
                    info.http_reads_without(query, self.missing)
                        .intersects(self.poll_reads())
                })
    }

    /// The reads of the HTTP mode, see `POLL_READS`.
    const fn poll_reads(&self) -> HttpReads {
        if self.signal_secs() == 0 {
            POLL_READS_NO_PAGES
        } else {
            POLL_READS
        }
    }

    /// Seconds between signal checks. 0: off. A value of 1 or 2 counts as `MIN_SIGNAL_SECS`.
    /// Telnet checks the signal itself on receivers with `CommApiVers` 0301 or higher. The
    /// reads over HTTP are in steps of 10 s; there 0 reads only `AppCommand0300`, not the
    /// signal pages.
    pub const fn signal_secs(&self) -> u16 {
        match self.signal_poll_secs {
            None => DEFAULT_SIGNAL_SECS,
            Some(0) => 0,
            Some(secs) if secs < MIN_SIGNAL_SECS => MIN_SIGNAL_SECS,
            Some(secs) => secs,
        }
    }

    /// Telnet checks the signal itself (`CommApiVers` 0301 or higher), or the version is not
    /// known yet.
    pub fn has_signal_check(&self) -> bool {
        self.heos().unwrap_or(true)
    }

    /// The receiver has a HEOS player.
    pub fn has_heos(&self) -> bool {
        matches!(self.heos(), Some(true))
    }

    /// The receiver has a HEOS player, or it is not known yet. The oldest receivers came before
    /// HEOS.
    pub fn may_have_heos(&self) -> bool {
        self.heos()
            .unwrap_or(!matches!(self.receiver_type, Some(ReceiverGeneration::Avr)))
    }

    /// The receiver has HEOS, from `CommApiVers`. `None` before the version is read.
    fn heos(&self) -> Option<bool> {
        self.comm_api_version
            .map(|_| matches!(self.generation(), SpecGeneration::Current))
    }

    /// The specs that the receiver follows (see `Info::spec_generation`). Before the receiver is
    /// read, from `CommApiVers` and the brand setting. `Unknown` if no spec is known for
    /// `CommApiVers`, or before it is read.
    pub fn generation(&self) -> SpecGeneration {
        if let Some(info) = &self.info {
            return info.spec_generation();
        }
        let version = self.comm_api_version.map(|CommApiVersion(v)| v);
        let brand = if self.is_denon {
            Brand::Denon
        } else {
            Brand::Marantz
        };
        SpecGeneration::of_version(version, brand)
    }

    /// The receiver may have the feature: a spec of its generation has it, or no spec is known.
    /// Tuner and Dirac come from the setup.
    pub fn offers(&self, feature: Feature) -> bool {
        match feature {
            Feature::Spec(spec) => self.generation().supports(spec),
            Feature::Dirac => self.has_dirac(),
            Feature::Tuner => self.tuner.unwrap_or(!matches!(self.has_tuner, Some(false))),
        }
    }

    /// The number of network audio favorites: 36 on Denon 2014 and 2015 receivers, `None` on
    /// receivers with HEOS, else 56.
    pub fn network_favorites(&self) -> Option<u8> {
        self.generation().network_presets()
    }

    /// The receiver has this quick select slot, counted from 1 (see `Info::has_quick_select`).
    /// True for all slots before the receiver is read.
    pub fn has_quick_select(&self, slot: u8) -> bool {
        self.info
            .as_ref()
            .is_none_or(|info| info.has_quick_select(slot))
    }

    /// The receiver may accept the command text (see `Info::offers_line`). True before the
    /// receiver is read.
    pub fn offers_line(&self, line: &str) -> bool {
        self.info.as_ref().is_none_or(|info| info.offers_line(line))
    }

    /// The receiver may have the value (see `Info::offers`). True before the receiver is read.
    pub fn offers_query(&self, query: Query) -> bool {
        self.info.as_ref().is_none_or(|info| info.offers(query))
    }

    /// Take what a device actor found on the receiver: the address and the host name, the receiver
    /// kind, the ids, the receiver values, the missing sources, the form of the tuner presets and
    /// the names of the presets and favorites. The other fields are settings and keep their
    /// values. `None` if nothing changed, else whether the receiver kind, the receiver values or,
    /// in HTTP mode, the missing sources changed: then the receiver can have other entities.
    ///
    /// Without `current`, `found` was sent before the newest setup: only the names and the form of
    /// the tuner presets are taken, since a setup does not set them.
    pub fn take_found(&mut self, found: Self, current: bool) -> Option<bool> {
        let new_form = found.tuner_presets_lettered.is_some()
            && self.tuner_presets_lettered != found.tuner_presets_lettered;
        let new_presets = !found.tuner_preset_names.is_empty()
            && self.tuner_preset_names != found.tuner_preset_names;
        let new_favorites = !found.network_favorite_names.is_empty()
            && self.network_favorite_names != found.network_favorite_names;
        if !current {
            if new_form {
                self.tuner_presets_lettered = found.tuner_presets_lettered;
            }
            if new_presets {
                self.tuner_preset_names = found.tuner_preset_names;
            }
            if new_favorites {
                self.network_favorite_names = found.network_favorite_names;
            }
            return (new_form || new_presets || new_favorites).then_some(false);
        }
        let kind = found.kind();
        let new_kind = kind != self.kind();
        let new_ids = self.add_ids(found.serial.as_deref(), found.mac.as_deref());
        let new_info = found.info.is_some() && self.info != found.info;
        let new_missing = self.missing != found.missing;
        if self.address == found.address
            && self.host == found.host
            && !new_kind
            && !new_ids
            && !new_info
            && !new_missing
            && !new_form
            && !new_presets
            && !new_favorites
        {
            return None;
        }
        if new_presets {
            self.tuner_preset_names = found.tuner_preset_names;
        }
        if new_favorites {
            self.network_favorite_names = found.network_favorite_names;
        }
        if new_info {
            self.info = found.info;
        }
        if new_form {
            self.tuner_presets_lettered = found.tuner_presets_lettered;
        }
        self.missing = found.missing;
        self.address = found.address;
        self.host = found.host;
        self.set_kind(kind);
        // With telnet, the missing sources do not change the entities.
        Some(new_kind || new_info || (new_missing && !self.use_telnet))
    }

    pub const fn kind(&self) -> ReceiverKind {
        ReceiverKind {
            receiver_type: self.receiver_type,
            update_method: self.update_method,
            comm_api_version: self.comm_api_version,
            has_tuner: self.has_tuner,
        }
    }

    pub const fn set_kind(&mut self, kind: ReceiverKind) {
        self.receiver_type = kind.receiver_type;
        self.update_method = kind.update_method;
        self.comm_api_version = kind.comm_api_version;
        self.has_tuner = kind.has_tuner;
    }
}

/// The Python integration reads a field that is `null` as the default. So does this driver.
fn true_if_null<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(true))
}

fn false_if_null<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(false))
}

fn step_if_null<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    Ok(Option::<f32>::deserialize(d)?.unwrap_or_else(default_volume_step))
}

/// A value that this version does not know reads as `None`, so the file still loads.
fn none_if_unknown<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    d: D,
) -> Result<Option<T>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value.and_then(|v| T::deserialize(v).ok()))
}

/// Like `none_if_unknown`, for a value with a default.
fn default_if_unknown<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> Result<T, D::Error> {
    none_if_unknown(d).map(Option::unwrap_or_default)
}

/// Whether the text is an IP address: IPv4, or IPv6 with or without a zone (`fe80::1%wlan0`).
pub fn is_ip(text: &str) -> bool {
    text.parse::<IpAddr>().is_ok()
        || text
            .split_once('%')
            .is_some_and(|(ip, zone)| !zone.is_empty() && ip.parse::<Ipv6Addr>().is_ok())
}

/// The 12 hexadecimal digits of a MAC address in uppercase. `:` and `-` between them are removed.
/// `None` if the text is not a MAC address.
pub fn parse_mac(text: &str) -> Option<[u8; 12]> {
    let mut digits = [0; 12];
    let mut count = 0;
    for b in text.bytes() {
        match b {
            b':' | b'-' => {}
            _ if b.is_ascii_hexdigit() && count < digits.len() => {
                digits[count] = b.to_ascii_uppercase();
                count += 1;
            }
            _ => return None,
        }
    }
    (count == digits.len()).then_some(digits)
}

/// A MAC address in the form of the configuration: 12 uppercase hexadecimal digits.
pub fn mac_text(text: &str) -> Option<String> {
    let digits = parse_mac(text)?;
    std::str::from_utf8(&digits).ok().map(str::to_owned)
}

/// An unknown id kind reads as the serial number, as in files that do not have it.
fn serial_if_unknown<'de, D: Deserializer<'de>>(d: D) -> Result<IdKind, D::Error> {
    Ok(none_if_unknown(d)?.unwrap_or_default())
}

fn none_if_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.filter(|s| !s.is_empty()))
}

/// A value that is not a MAC address reads as `None`.
fn mac_if_valid<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.and_then(|m| mac_text(&m)))
}

fn timeout_if_null<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    Ok(Option::<u32>::deserialize(d)?.unwrap_or_else(default_timeout))
}

const fn default_true() -> bool {
    true
}

const fn default_volume_step() -> f32 {
    0.5
}

const fn default_timeout() -> u32 {
    2000
}

#[allow(clippy::trivially_copy_pass_by_ref)] // The signature that serde needs.
fn is_default(v: &MissingSources) -> bool {
    *v == MissingSources::default()
}

/// The sources that are missing in `a` or in `b`.
pub const fn missing_union(a: MissingSources, b: MissingSources) -> MissingSources {
    MissingSources {
        appcommand0300: a.appcommand0300 || b.appcommand0300,
        tone_control: a.tone_control || b.tone_control,
        video_page: a.video_page || b.video_page,
        channels_page: a.channels_page || b.channels_page,
    }
}

/// Version of the backup format.
const BACKUP_VERSION: u8 = 1;

#[derive(Serialize)]
struct BackupRef<'a> {
    version: u8,
    devices: &'a [DeviceConfig],
}

#[derive(Deserialize)]
struct Backup {
    version: u8,
    devices: Vec<DeviceConfig>,
}

/// Serialize the devices for a backup.
pub fn backup_json(devices: &[DeviceConfig]) -> Option<String> {
    serde_json::to_string(&BackupRef {
        version: BACKUP_VERSION,
        devices,
    })
    .ok()
}

/// Read the devices of a backup. Returns `None` if the text is not a valid backup.
pub fn parse_backup(text: &str) -> Option<Vec<DeviceConfig>> {
    let backup: Backup = serde_json::from_str(text.trim()).ok()?;
    if backup.version != BACKUP_VERSION || backup.devices.is_empty() {
        return None;
    }
    let mut keys: Vec<&str> = Vec::with_capacity(backup.devices.len());
    for device in &backup.devices {
        if device.id.is_empty() || keys.contains(&device.id.as_str()) {
            return None;
        }
        keys.push(&device.id);
    }
    Some(backup.devices)
}

/// Driver settings that are not part of a device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverSettings {
    /// Log debug messages of the driver and the library.
    #[serde(default)]
    pub debug_log: bool,
}

/// Load the driver settings. A missing or broken file gives the defaults.
///
/// It is a free function, because the logger needs the settings before the store exists.
pub fn load_settings(dir: &Path) -> DriverSettings {
    read_json(&dir.join(SETTINGS_FILE))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Reads and writes the configuration files.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    dir: PathBuf,
    writer: Arc<Writer>,
}

impl ConfigStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            writer: Arc::new(Writer::new()),
        }
    }

    /// Wait until all queued writes are done. For tests.
    #[cfg(test)]
    pub(crate) fn flush(&self) {
        self.writer.flush();
    }

    /// Load the configured devices. A missing file gives an empty list.
    ///
    /// A broken file is moved to `config.json.bad-<unix time>`, so the next save does not
    /// overwrite it.
    pub fn load_devices(&self) -> Vec<DeviceConfig> {
        self.writer.flush();
        let path = self.dir.join(CONFIG_FILE);
        match read_json(&path) {
            Ok(Some(devices)) => devices,
            Ok(None) => Vec::new(),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let bad = self.dir.join(format!("{CONFIG_FILE}.bad-{secs}"));
                match fs::rename(&path, &bad) {
                    Ok(()) => error!(
                        "Invalid {}: {e}. The file was moved to {}",
                        path.display(),
                        bad.display()
                    ),
                    Err(re) => error!(
                        "Invalid {}: {e}. Could not move the file to {}: {re}",
                        path.display(),
                        bad.display()
                    ),
                }
                Vec::new()
            }
            Err(e) => {
                error!("Error reading {}: {e}", path.display());
                Vec::new()
            }
        }
    }

    /// Queue the save of the configured devices. The write runs on the writer thread: the caller
    /// does not wait for the disk, and gets the result from the returned [`SaveWait`].
    pub fn save_devices(&self, devices: &[&DeviceConfig]) -> io::Result<SaveWait> {
        let data = serde_json::to_vec_pretty(&devices).map_err(io::Error::other)?;
        let (tx, rx) = oneshot::channel();
        self.writer.send(Job::Write {
            path: self.dir.join(CONFIG_FILE),
            data,
            private: true,
            done: Some(tx),
        });
        info!("Saving the configuration with {} device(s)", devices.len());
        Ok(SaveWait(rx))
    }

    /// Queue the save of the configured devices and return at once. A write error is only
    /// logged. Use it where no caller waits for the result.
    pub fn queue_devices(&self, devices: &[&DeviceConfig]) -> io::Result<()> {
        self.write_json(self.dir.join(CONFIG_FILE), devices, true)
    }

    /// Queue the save of the driver settings. A write error is only logged.
    pub fn save_settings(&self, settings: DriverSettings) -> io::Result<()> {
        self.write_json(self.dir.join(SETTINGS_FILE), &settings, false)
    }

    /// Serialize the value and queue the write. Returns only serialization errors.
    fn write_json<T: Serialize + ?Sized>(
        &self,
        path: PathBuf,
        value: &T,
        private: bool,
    ) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        self.writer.send(Job::Write {
            path,
            data,
            private,
            done: None,
        });
        Ok(())
    }
}

/// The result of a write that the writer thread does.
#[derive(Debug)]
pub struct SaveWait(oneshot::Receiver<io::Result<()>>);

impl SaveWait {
    /// Wait for the end of the write, without blocking the thread.
    pub async fn wait(self) -> io::Result<()> {
        self.0.await.map_err(io::Error::other)?
    }

    /// Wait for the end of the write on this thread. For tests.
    #[cfg(test)]
    pub fn wait_blocking(self) -> io::Result<()> {
        self.0.blocking_recv().map_err(io::Error::other)?
    }
}

/// File changes for the background thread.
#[derive(Debug)]
enum Job {
    Write {
        path: PathBuf,
        data: Vec<u8>,
        private: bool,
        /// Gets the result. Without it, errors are only logged.
        done: Option<oneshot::Sender<io::Result<()>>>,
    },
    /// Answer when all earlier jobs are done.
    Flush(mpsc::SyncSender<()>),
}

impl Job {
    fn run(self) {
        match self {
            Self::Write {
                path,
                data,
                private,
                done,
            } => {
                let result = write_file(&path, &data, private);
                if let Err(e) = &result {
                    error!("Error writing {}: {e}", path.display());
                }
                if let Some(done) = done {
                    let _ = done.send(result);
                }
            }
            Self::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Runs the jobs in order on one thread. When the last [`ConfigStore`] is dropped, the queued
/// jobs are done before the thread ends.
#[derive(Debug)]
struct Writer {
    tx: Option<mpsc::Sender<Job>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Writer {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        match thread::Builder::new()
            .name("config-writer".into())
            .spawn(move || rx.into_iter().for_each(Job::run))
        {
            Ok(thread) => Self {
                tx: Some(tx),
                thread: Some(thread),
            },
            Err(e) => {
                error!("Could not start the config writer thread, writing directly: {e}");
                Self {
                    tx: None,
                    thread: None,
                }
            }
        }
    }

    /// Queue the job. Without the thread, the job runs on the caller thread.
    fn send(&self, job: Job) {
        let job = match &self.tx {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        job.run();
    }

    /// Wait until all queued jobs are done.
    fn flush(&self) {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Job::Flush(tx));
        let _ = rx.recv();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the channel ends the thread after the queued jobs.
        self.tx = None;
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!("The config writer thread failed");
        }
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_file(
    path: &Path,
    data: &[u8],
    // Only Unix has file modes.
    #[cfg_attr(not(unix), expect(unused_variables))] private: bool,
) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    // A file left from a failed write keeps its old mode, so always create a new file.
    if let Err(e) = fs::remove_file(&tmp)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e);
    }
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // Make the rename durable.
    #[cfg(unix)]
    {
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// The configuration as after a read of a current receiver that lists this many quick selects.
#[cfg(test)]
pub(crate) fn read_with_quick_selects(cfg: DeviceConfig, count: Option<u8>) -> DeviceConfig {
    let mut cfg = read_with_version(cfg, "0301");
    if let Some(info) = &mut cfg.info {
        info.quick_select_count = count;
    }
    cfg
}

/// The configuration as after a read of a receiver with this `CommApiVers`. The receiver lists no
/// features, so the library offers what the spec generation has.
#[cfg(test)]
pub(crate) fn read_with_version(cfg: DeviceConfig, version: &str) -> DeviceConfig {
    use denon_marantz::{Info, ReceiverType, TunerInfo, TypeProof, UpdateMethod, Zone};
    let info = Info {
        host: cfg.address.clone(),
        receiver_type: ReceiverType::AvrX2016,
        port: 8080,
        type_proof: TypeProof::Read,
        update_method: UpdateMethod::AppCommand,
        brand: if cfg.is_denon {
            Brand::Denon
        } else {
            Brand::Marantz
        },
        manufacturer: None,
        model_name: None,
        serial_number: None,
        friendly_name: String::new(),
        comm_api_version: Some(version.to_owned()),
        mac_address: None,
        dirac_live: false,
        tuner: TunerInfo::Unknown,
        quick_select_count: None,
        queries: None,
        zones: vec![Zone::Main],
        zone_count: None,
        features: None,
        ranges: denon_marantz::DeviceRanges::default(),
    };
    DeviceConfig {
        comm_api_version: CommApiVersion::parse(version),
        info: Some(info),
        ..cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceConfig {
        DeviceConfig {
            id: "SERIAL-1".into(),
            id_kind: IdKind::Serial,
            serial: None,
            mac: None,
            name: "Living Room".into(),
            address: "192.0.2.10".into(),
            host: None,
            support_sound_mode: true,
            show_all_inputs: true,
            use_telnet: false,
            update_audyssey: false,
            zone2: false,
            zone3: false,
            volume_step: 1.0,
            timeout: 2000,
            is_denon: false,
            is_dirac_supported: Some(true),
            signal_poll_secs: Some(10),
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

    /// A file written by the Python integration.
    const PYTHON_CONFIG: &str = r#"[{"id": "SERIAL0000001", "name": "Denon AVC-A10H", "address": "192.0.2.6", "support_sound_mode": true, "show_all_inputs": false, "use_telnet": true, "update_audyssey": false, "zone2": false, "zone3": false, "volume_step": 1.0, "timeout": 2000, "is_denon": true, "is_dirac_supported": true}]"#;

    #[test]
    fn python_config_loads_and_keeps_its_fields() {
        let devices: Vec<DeviceConfig> = serde_json::from_str(PYTHON_CONFIG).expect("json");
        assert_eq!(1, devices.len());
        let d = &devices[0];
        assert_eq!("SERIAL0000001", d.key());
        assert_eq!("192.0.2.6", d.address);
        assert!(d.use_telnet && d.is_denon && d.has_dirac());
        assert_eq!(
            (None, DEFAULT_SIGNAL_SECS),
            (d.signal_poll_secs, d.signal_secs())
        );
        // The driver writes the same fields, so the Python integration can read the file.
        let text = serde_json::to_string(&devices).expect("json");
        let python: serde_json::Value = serde_json::from_str(PYTHON_CONFIG).expect("json");
        let ours: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(python, ours);
    }

    #[test]
    fn the_ids_are_read_and_written() {
        let python: Vec<DeviceConfig> = serde_json::from_str(PYTHON_CONFIG).expect("json");
        let d = &python[0];
        assert_eq!(IdKind::Serial, d.id_kind);
        assert_eq!((Some("SERIAL0000001"), None), (d.serial(), d.mac()));

        let text = r#"{"id": "0123456789AB", "id_kind": "mac", "serial": "S1", "mac": "01:23:45:67:89:ab", "name": "N", "address": "a"}"#;
        let d: DeviceConfig = serde_json::from_str(text).expect("json");
        assert_eq!(IdKind::Mac, d.id_kind);
        assert_eq!((Some("S1"), Some("0123456789AB")), (d.serial(), d.mac()));
        let again: DeviceConfig =
            serde_json::from_str(&serde_json::to_string(&d).expect("json")).expect("json");
        assert_eq!(d, again);

        // A kind that this version does not know is the serial number. Empty ids are none.
        let text = r#"{"id": "S1", "id_kind": "other", "serial": "", "mac": null, "name": "N", "address": "a"}"#;
        let d: DeviceConfig = serde_json::from_str(text).expect("json");
        assert_eq!((IdKind::Serial, None, None), (d.id_kind, d.serial, d.mac));
    }

    #[test]
    fn mac_addresses_are_read_with_or_without_separators() {
        let digits = Some(*b"0123456789AB");
        assert_eq!(digits, parse_mac("0123456789ab"));
        assert_eq!(digits, parse_mac("01:23:45:67:89:AB"));
        assert_eq!(digits, parse_mac("01-23-45-67-89-ab"));
        for bad in [
            "",
            "0123456789A",
            "0123456789ABC",
            "0123456789AG",
            "01 23 45 67 89 AB",
        ] {
            assert_eq!(None, parse_mac(bad), "{bad}");
        }
        assert_eq!(
            Some("0123456789AB".to_owned()),
            mac_text("01:23:45:67:89:ab")
        );
    }

    #[test]
    fn the_same_receiver_is_found_by_the_mac_address_first() {
        let mut d = device();
        // The id is the serial number, no MAC address is stored.
        assert_eq!(Some(true), d.same_receiver(Some("SERIAL-1"), None));
        assert_eq!(
            Some(false),
            d.same_receiver(Some("OTHER"), Some("0123456789AB"))
        );
        assert_eq!(None, d.same_receiver(None, Some("0123456789AB")));

        assert!(d.add_ids(Some("SERIAL-1"), Some("0123456789ab")));
        assert!(!d.add_ids(Some("OTHER"), Some("AABBCCDDEEFF")));
        assert_eq!(Some("0123456789AB"), d.mac());
        // Both MAC addresses are known: the serial number is not compared.
        assert_eq!(Some(true), d.same_receiver(None, Some("0123456789ab")));
        assert_eq!(
            Some(false),
            d.same_receiver(Some("SERIAL-1"), Some("AABBCCDDEEFF"))
        );
        assert_eq!(Some(true), d.same_receiver(Some("SERIAL-1"), None));
        assert_eq!(None, d.same_receiver(None, None));
    }

    #[test]
    fn the_receiver_kind_is_kept_and_a_python_config_has_none() {
        let python: Vec<DeviceConfig> = serde_json::from_str(PYTHON_CONFIG).expect("json");
        assert_eq!(
            (None, None),
            (python[0].receiver_type, python[0].update_method)
        );
        // A file without the values stays without them.
        let text = serde_json::to_string(&python[0]).expect("json");
        assert!(
            !text.contains("receiver_type")
                && !text.contains("update_method")
                && !text.contains("comm_api_version"),
            "{text}"
        );

        let d = DeviceConfig {
            receiver_type: Some(ReceiverGeneration::AvrX2016),
            update_method: Some(HttpReadMethod::AppCommand),
            comm_api_version: CommApiVersion::parse("0301"),
            ..device()
        };
        let text = serde_json::to_string(&d).expect("json");
        assert!(
            text.contains(r#""receiver_type":"avr_x2016""#)
                && text.contains(r#""update_method":"app_command""#)
                && text.contains(r#""comm_api_version":"0301""#),
            "{text}"
        );
        assert_eq!(d, serde_json::from_str(&text).expect("json"));
    }

    #[test]
    fn the_tuner_option_wins_over_deviceinfo() {
        let with = |tuner, has_tuner| {
            DeviceConfig {
                has_tuner,
                tuner,
                ..device()
            }
            .offers(Feature::Tuner)
        };
        assert!(with(None, None) && with(None, Some(true)));
        assert!(!with(None, Some(false)));
        assert!(!with(Some(false), Some(true)) && with(Some(true), Some(false)));
    }

    #[test]
    fn the_comm_api_version_is_four_digits_or_none() {
        let v = |text| CommApiVersion::parse(text);
        assert_eq!(Some(CommApiVersion(301)), v("0301"));
        assert_eq!(Some(CommApiVersion::MISSING), v("none"));
        for text in ["301", "03010", "03.1", "", "x301"] {
            assert_eq!(None, v(text), "{text}");
        }
        for (version, text) in [
            (CommApiVersion(210), r#""0210""#),
            (CommApiVersion::MISSING, r#""none""#),
        ] {
            assert_eq!(text, serde_json::to_string(&version).expect("json"));
            assert_eq!(
                version,
                serde_json::from_str::<CommApiVersion>(text).expect("json")
            );
        }
    }

    #[test]
    fn only_a_receiver_that_was_not_read_yet_may_have_heos() {
        let may = |comm_api_version, receiver_type| {
            DeviceConfig {
                receiver_type,
                comm_api_version,
                ..device()
            }
            .may_have_heos()
        };
        assert!(may(None, None));
        assert!(may(None, Some(ReceiverGeneration::AvrX)));
        assert!(!may(None, Some(ReceiverGeneration::Avr)));
        assert!(!may(
            Some(CommApiVersion::MISSING),
            Some(ReceiverGeneration::AvrX)
        ));
        assert!(!may(CommApiVersion::parse("0300"), None));
        assert!(may(CommApiVersion::parse("0301"), None));
        assert!(may(CommApiVersion::parse("0400"), None));
    }

    #[test]
    fn an_unknown_receiver_kind_reads_as_none() {
        // `quick_select_count`, `hdmi_output_over_http` and `listed_queries` are old fields: a file
        // with them still loads.
        let d: DeviceConfig = serde_json::from_str(
            r#"{"id":"A","name":"C","address":"h","receiver_type":"avr_z","update_method":7,"hdmi_output_over_http":"yes","quick_select_count":4,"listed_queries":[1],"comm_api_version":"301"}"#,
        )
        .expect("json");
        assert_eq!(ReceiverKind::default(), d.kind());
    }

    #[test]
    fn old_http_fields_load_and_are_not_saved() {
        let d: DeviceConfig = serde_json::from_str(
            r#"{"id":"A","name":"C","address":"h","receiver_type":"avr_x2016","hdmi_output_over_http":true,"listed_queries":{"audyssey":true}}"#,
        )
        .expect("json");
        assert_eq!(Some(ReceiverGeneration::AvrX2016), d.receiver_type);
        let text = serde_json::to_string(&d).expect("json");
        assert!(
            !text.contains("hdmi_output_over_http") && !text.contains("listed_queries"),
            "{text}"
        );
    }

    #[test]
    fn a_configuration_without_a_host_name_is_saved_without_it() {
        let d: DeviceConfig =
            serde_json::from_str(r#"{"id":"A","name":"C","address":"192.0.2.6"}"#).expect("json");
        assert_eq!(None, d.host);
        assert_eq!("192.0.2.6", d.shown_address());
        let text = serde_json::to_string(&d).expect("json");
        assert!(!text.contains(r#""host""#), "{text}");
    }

    #[test]
    fn a_host_name_is_saved_and_loaded() {
        let d = DeviceConfig {
            host: Some("avr.local".into()),
            ..device()
        };
        let text = serde_json::to_string(&d).expect("json");
        assert!(text.contains(r#""host":"avr.local""#), "{text}");
        let loaded: DeviceConfig = serde_json::from_str(&text).expect("json");
        assert_eq!(d, loaded);
        assert_eq!("avr.local", loaded.shown_address());
        assert_eq!("192.0.2.10", loaded.address);
    }

    #[test]
    fn is_ip_accepts_only_ip_addresses() {
        for ip in [
            "192.0.2.6",
            "fe80::1",
            "fe80::1%en0",
            "fe80::1%wlan0",
            "::1",
        ] {
            assert!(is_ip(ip), "{ip}");
        }
        for name in [
            "avr.local",
            "localhost",
            "avr",
            "192.0.2",
            "foo:bar",
            "1:2:3",
            "fe80::1%",
            "192.0.2.6.example",
        ] {
            assert!(!is_ip(name), "{name}");
        }
    }

    #[test]
    fn a_new_address_keeps_the_host_name() {
        let with = |address: &str, host: Option<&str>| DeviceConfig {
            address: address.into(),
            host: host.map(Into::into),
            ..device()
        };
        let set = |mut cfg: DeviceConfig| {
            cfg.set_address("192.0.2.20".into());
            (cfg.address, cfg.host)
        };
        let moved = |host: Option<&str>| ("192.0.2.20".to_owned(), host.map(str::to_owned));
        // A stored host name stays.
        assert_eq!(moved(Some("avr")), set(with("192.0.2.10", Some("avr"))));
        assert_eq!(moved(Some("avr")), set(with("old.local", Some("avr"))));
        // An old address that is a host name becomes the host name.
        assert_eq!(moved(Some("old.local")), set(with("old.local", None)));
        // An old IP address is not kept.
        assert_eq!(moved(None), set(with("192.0.2.10", None)));
        assert_eq!(moved(None), set(with("fe80::1", None)));
        // An empty old address is not kept.
        assert_eq!(moved(None), set(with("", None)));
        // An IPv6 address in brackets is not kept.
        assert_eq!(moved(None), set(with("[::1]", None)));
    }

    #[test]
    fn the_host_name_of_a_connect_is_taken() {
        let mut cfg = device();
        let found = DeviceConfig {
            address: "192.0.2.20".into(),
            host: Some("avr.local".into()),
            ..device()
        };
        assert_eq!(Some(false), cfg.take_found(found.clone(), true));
        assert_eq!(found, cfg);
        assert_eq!(None, cfg.take_found(found, true));
        let host_only = DeviceConfig {
            host: Some("other.local".into()),
            ..cfg.clone()
        };
        assert_eq!(Some(false), cfg.take_found(host_only, true));
        assert_eq!(Some("other.local"), cfg.host.as_deref());
    }

    #[test]
    fn found_values_are_taken_and_settings_are_kept() {
        let old: DeviceConfig =
            serde_json::from_str(r#"{"id":"A","name":"C","address":"h","use_telnet":false}"#)
                .expect("json");
        let mut cfg = old.clone();
        assert_eq!(None, cfg.take_found(old.clone(), true));
        let mut found = old;
        found.use_telnet = true;
        found.address = "h2".into();
        found.receiver_type = Some(ReceiverGeneration::AvrX);
        assert_eq!(Some(true), cfg.take_found(found, true));
        assert_eq!(("h2", false), (cfg.address.as_str(), cfg.use_telnet));
        let info =
            r#"{"id":"A","name":"C","address":"h2","receiver_type":"avr_x","info":{"host":"h2"}}"#;
        // A value that does not load is not taken: `None` keeps the stored one.
        let found: DeviceConfig = serde_json::from_str(info).expect("json");
        assert_eq!(None, cfg.take_found(found.clone(), true));
        // The form of the tuner presets is taken, and `None` keeps it.
        let lettered = DeviceConfig {
            tuner_presets_lettered: Some(true),
            ..found.clone()
        };
        assert_eq!(Some(false), cfg.take_found(lettered, true));
        assert_eq!(None, cfg.take_found(found.clone(), true));
        assert_eq!(Some(true), cfg.tuner_presets_lettered);
        // New receiver values can change the entities.
        let read = DeviceConfig {
            info: read_with_version(found.clone(), "0301").info,
            ..found
        };
        assert_eq!(Some(true), cfg.take_found(read.clone(), true));
        // Missing sources are taken, also when they are cleared, and can change the entities.
        let missing = DeviceConfig {
            missing: MissingSources {
                video_page: true,
                ..MissingSources::default()
            },
            ..read.clone()
        };
        assert_eq!(Some(true), cfg.take_found(missing.clone(), true));
        assert_eq!(missing.missing, cfg.missing);
        assert_eq!(Some(true), cfg.take_found(read, true));
        assert_eq!(MissingSources::default(), cfg.missing);
        // With telnet they are taken, but the entities stay the same.
        cfg.use_telnet = true;
        assert_eq!(Some(false), cfg.take_found(missing.clone(), true));
        assert_eq!(missing.missing, cfg.missing);
        // From before the newest setup, only the names and the preset form are taken.
        let before = cfg.clone();
        let old = DeviceConfig {
            address: "h3".into(),
            missing: MissingSources::default(),
            tuner_presets_lettered: Some(false),
            network_favorite_names: vec!["Radio".into()],
            ..missing
        };
        assert_eq!(Some(false), cfg.take_found(old.clone(), false));
        assert_eq!(None, cfg.take_found(old, false));
        let expected = DeviceConfig {
            tuner_presets_lettered: Some(false),
            network_favorite_names: vec!["Radio".into()],
            ..before
        };
        assert_eq!(expected, cfg);
    }

    #[test]
    fn missing_sources_are_saved_only_when_set() {
        let d: DeviceConfig =
            serde_json::from_str(r#"{"id":"A","name":"C","address":"h"}"#).expect("json");
        assert_eq!(MissingSources::default(), d.missing);
        assert!(!serde_json::to_string(&d).expect("json").contains("missing"));
        let d: DeviceConfig = serde_json::from_str(
            r#"{"id":"A","name":"C","address":"h","missing":{"channels_page":true,"later":1}}"#,
        )
        .expect("json");
        assert!(d.missing.channels_page && !d.missing.video_page);
        // A value that does not load does not stop the receiver from loading.
        for bad in ["null", "true", r#"{"video_page":"yes"}"#] {
            let text = format!(r#"{{"id":"A","name":"C","address":"h","missing":{bad}}}"#);
            let d: DeviceConfig = serde_json::from_str(&text).expect("json");
            assert_eq!(MissingSources::default(), d.missing, "{bad}");
        }
        let text = serde_json::to_string(&d).expect("json");
        assert!(text.contains(r#""channels_page":true"#), "{text}");
    }

    #[test]
    fn missing_sources_add_up() {
        let video = MissingSources {
            video_page: true,
            ..MissingSources::default()
        };
        let tone = MissingSources {
            tone_control: true,
            ..MissingSources::default()
        };
        let both = missing_union(video, tone);
        assert!(both.video_page && both.tone_control);
        assert!(!both.appcommand0300 && !both.channels_page);
        assert_eq!(video, missing_union(video, MissingSources::default()));
    }

    #[test]
    fn receiver_values_of_another_library_version_read_as_none() {
        let d: DeviceConfig =
            serde_json::from_str(r#"{"id":"A","name":"C","address":"h","info":{"host":"h"}}"#)
                .expect("json");
        assert_eq!(None, d.info);
    }

    #[test]
    fn old_python_config_gets_defaults() {
        let d: DeviceConfig =
            serde_json::from_str(r#"{"id":"A","name":"C","address":"h"}"#).expect("json");
        assert!(d.support_sound_mode && d.use_telnet && d.is_denon);
        assert!(!d.show_all_inputs && !d.has_dirac());
        assert!((d.volume_step - 0.5).abs() < f32::EPSILON);
        assert_eq!(2000, d.timeout);
    }

    #[test]
    fn the_signal_time_has_a_default_and_a_lowest_value() {
        let secs = |value| {
            DeviceConfig {
                signal_poll_secs: value,
                ..device()
            }
            .signal_secs()
        };
        assert_eq!(DEFAULT_SIGNAL_SECS, secs(None));
        assert_eq!(0, secs(Some(0)));
        assert_eq!(MIN_SIGNAL_SECS, secs(Some(1)));
        assert_eq!(MIN_SIGNAL_SECS, secs(Some(2)));
        assert_eq!(MIN_SIGNAL_SECS, secs(Some(MIN_SIGNAL_SECS)));
        assert_eq!(60, secs(Some(60)));
        // 0 is written: it differs from the default.
        let text = serde_json::to_string(&DeviceConfig {
            signal_poll_secs: Some(0),
            ..device()
        })
        .expect("json");
        assert!(text.contains(r#""signal_poll_secs":0"#));
    }

    #[test]
    fn only_receivers_from_0301_have_the_signal_check() {
        let check = |version: Option<&str>| {
            DeviceConfig {
                comm_api_version: version.and_then(CommApiVersion::parse),
                ..device()
            }
            .has_signal_check()
        };
        assert!(check(None));
        assert!(check(Some("0301")));
        assert!(!check(Some("0300")));
        assert!(!read_with_version(device(), "0300").has_signal_check());
        assert!(read_with_version(device(), "0301").has_signal_check());
    }

    #[test]
    fn null_fields_get_the_defaults_like_in_the_python_integration() {
        let text = r#"{"id":"A","name":"C","address":"h","support_sound_mode":null,
            "show_all_inputs":null,"use_telnet":null,"update_audyssey":null,"zone2":null,
            "zone3":null,"volume_step":null,"timeout":null,"is_denon":null,
            "is_dirac_supported":null,"signal_poll_secs":null}"#;
        let d: DeviceConfig = serde_json::from_str(text).expect("json");
        assert!(d.support_sound_mode && d.use_telnet && d.is_denon);
        assert!(!d.show_all_inputs && !d.update_audyssey && !d.zone2 && !d.zone3);
        assert!((d.volume_step - 0.5).abs() < f32::EPSILON);
        assert_eq!((2000, None), (d.timeout, d.signal_poll_secs));
        assert!(!d.has_dirac());
    }

    #[test]
    fn the_old_telnet_for_events_field_is_ignored() {
        let text = r#"{"id":"A","name":"C","address":"h","use_telnet":false,
            "use_telnet_for_events":true}"#;
        let d: DeviceConfig = serde_json::from_str(text).expect("json");
        assert!(!d.use_telnet);
    }

    #[test]
    fn settings_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "uc-intg-denonmarantz-test-settings-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("dir");
        assert_eq!(DriverSettings::default(), load_settings(&dir));

        let store = ConfigStore::new(&dir);
        let settings = DriverSettings { debug_log: true };
        store.save_settings(settings).expect("save");
        store.writer.flush();
        assert_eq!(settings, load_settings(&dir));

        // A broken file gives the defaults.
        fs::write(dir.join(SETTINGS_FILE), b"{").expect("write");
        assert_eq!(DriverSettings::default(), load_settings(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("uc-intg-denonmarantz-test-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let store = ConfigStore::new(&dir);
        assert_eq!(Vec::<DeviceConfig>::new(), store.load_devices());

        // A file left from a failed write must not keep its mode.
        fs::write(dir.join("config.json.tmp"), b"x").expect("tmp");
        let devices = vec![device()];
        store
            .save_devices(&[&devices[0]])
            .expect("save")
            .wait_blocking()
            .expect("written");
        assert_eq!(devices, store.load_devices());

        store.queue_devices(&[]).expect("queue");
        store.writer.flush();
        assert_eq!(Vec::<DeviceConfig>::new(), store.load_devices());
        store.queue_devices(&[&devices[0]]).expect("queue");
        store.writer.flush();
        assert_eq!(devices, store.load_devices());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(CONFIG_FILE))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(0o600, mode & 0o777);
        }

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn backup_round_trip() {
        let devices = vec![device()];
        let text = backup_json(&devices).expect("backup");
        assert_eq!(Some(devices), parse_backup(&format!("  {text}\n")));
    }

    #[test]
    fn invalid_backups_are_rejected() {
        let valid = backup_json(&[device()]).expect("backup");
        assert_eq!(None, parse_backup(""));
        assert_eq!(None, parse_backup("{broken"));
        assert_eq!(None, parse_backup(&valid[..valid.len() - 2]));
        assert_eq!(
            None,
            parse_backup(&valid.replace("\"version\":1", "\"version\":2"))
        );
        assert_eq!(None, parse_backup(r#"{"version":1,"devices":[]}"#));

        // Two devices with the same key.
        let one = serde_json::to_string(&device()).expect("device");
        let twice = format!(r#"{{"version":1,"devices":[{one},{one}]}}"#);
        assert_eq!(None, parse_backup(&twice));
    }

    #[test]
    fn broken_config_is_moved() {
        let dir = std::env::temp_dir().join(format!(
            "uc-intg-denonmarantz-test-bad-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(CONFIG_FILE), b"{broken").expect("write");
        let store = ConfigStore::new(&dir);
        assert_eq!(Vec::<DeviceConfig>::new(), store.load_devices());
        assert!(!dir.join(CONFIG_FILE).exists());

        let bad: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.json.bad-")
            })
            .collect();
        assert_eq!(1, bad.len());
        assert_eq!(
            b"{broken".as_slice(),
            fs::read(bad[0].path()).expect("read")
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
