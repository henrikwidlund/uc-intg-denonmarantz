// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Configured receivers and per-device cache files.
//!
//! Files are written to a temporary file first and then renamed, so a crash never leaves a broken
//! file. The configuration file is only readable by the owner.
//!
//! The data is serialized on the caller thread. One background thread writes the files in order,
//! so the actor thread does not wait for the disk.

use log::{error, info};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

const CONFIG_FILE: &str = "config.json";
const SETTINGS_FILE: &str = "settings.json";

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
    /// IP address or host name.
    pub address: String,
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
    /// Volume step in dB for volume up and down.
    #[serde(default = "default_volume_step", deserialize_with = "step_if_null")]
    pub volume_step: f32,
    /// Not used. Kept for the Python integration.
    #[serde(default = "default_timeout", deserialize_with = "timeout_if_null")]
    pub timeout: u32,
    #[serde(default = "default_true", deserialize_with = "true_if_null")]
    pub is_denon: bool,
    #[serde(default)]
    pub is_dirac_supported: Option<bool>,
    /// Seconds between signal reads. 0 turns the timer off.
    #[serde(
        default,
        skip_serializing_if = "is_zero",
        deserialize_with = "zero_if_null"
    )]
    pub signal_poll_secs: u16,
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
    /// The receiver reads and sets the HDMI output over HTTP. It is found at the setup and checked
    /// at each connect.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub hdmi_output_over_http: Option<bool>,
    /// Number of quick select (smart select on Marantz) slots. It is found at the setup and checked
    /// at each connect.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub quick_select_count: Option<u8>,
    /// The `AppCommand` queries that the receiver lists. `None` if it has no list: then it is asked
    /// for all of them. It is found at the setup and checked at each connect.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub listed_queries: Option<HttpQueries>,
    /// The receiver has a HEOS player for network audio. It is found at the setup, or at the
    /// first connect if it is missing.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "none_if_unknown"
    )]
    pub heos: Option<bool>,
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

/// The `AppCommand` queries that the receiver lists in `Deviceinfo.xml`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpQueries {
    pub tone_control: bool,
    pub audyssey: bool,
    pub video_info: bool,
    pub audio_info: bool,
    pub input_signal: bool,
    pub active_speaker: bool,
    pub output_settings: bool,
}

/// What the driver found on the receiver. Entities and their options depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReceiverKind {
    pub receiver_type: Option<ReceiverGeneration>,
    pub update_method: Option<HttpReadMethod>,
    pub hdmi_output_over_http: Option<bool>,
    pub quick_select_count: Option<u8>,
    pub listed_queries: Option<HttpQueries>,
    pub heos: Option<bool>,
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

impl DeviceConfig {
    /// Key for the entity ids.
    pub fn key(&self) -> &str {
        &self.id
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

    /// The receiver is read with `AppCommand`: tone control and Audyssey come over HTTP too.
    pub const fn reads_app_command(&self) -> bool {
        matches!(self.update_method, Some(HttpReadMethod::AppCommand))
    }

    /// The signal comes over HTTP. This is tested only on the 2016 generation.
    pub const fn reads_signal(&self) -> bool {
        self.reads_app_command() && matches!(self.receiver_type, Some(ReceiverGeneration::AvrX2016))
    }

    /// The HDMI output comes over HTTP.
    pub const fn reads_hdmi_output(&self) -> bool {
        matches!(self.hdmi_output_over_http, Some(true))
    }

    /// Tone control, bass and treble come over HTTP.
    pub const fn reads_tone_control(&self) -> bool {
        self.reads_app_command()
            && match self.listed_queries {
                Some(q) => q.tone_control,
                None => true,
            }
    }

    /// `MultEq` and Dynamic EQ come over HTTP.
    pub const fn reads_audyssey(&self) -> bool {
        self.reads_app_command()
            && match self.listed_queries {
                Some(q) => q.audyssey,
                None => true,
            }
    }

    /// The audio signal and the sampling rate come over HTTP.
    pub const fn reads_audio_info(&self) -> bool {
        self.reads_signal()
            && match self.listed_queries {
                Some(q) => q.audio_info,
                None => true,
            }
    }

    /// The receiver has a HEOS player.
    pub const fn has_heos(&self) -> bool {
        matches!(self.heos, Some(true))
    }

    /// The receiver has a HEOS player, or it is not known yet. The oldest receivers came before
    /// HEOS.
    pub const fn may_have_heos(&self) -> bool {
        match self.heos {
            Some(heos) => heos,
            None => !matches!(self.receiver_type, Some(ReceiverGeneration::Avr)),
        }
    }

    /// The receiver has this quick select slot, counted from 1. Without a count, all slots.
    pub const fn has_quick_select(&self, slot: u8) -> bool {
        match self.quick_select_count {
            Some(count) => slot <= count,
            None => true,
        }
    }

    pub const fn kind(&self) -> ReceiverKind {
        ReceiverKind {
            receiver_type: self.receiver_type,
            update_method: self.update_method,
            hdmi_output_over_http: self.hdmi_output_over_http,
            quick_select_count: self.quick_select_count,
            listed_queries: self.listed_queries,
            heos: self.heos,
        }
    }

    pub const fn set_kind(&mut self, kind: ReceiverKind) {
        self.receiver_type = kind.receiver_type;
        self.update_method = kind.update_method;
        self.hdmi_output_over_http = kind.hdmi_output_over_http;
        self.quick_select_count = kind.quick_select_count;
        self.listed_queries = kind.listed_queries;
        self.heos = kind.heos;
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

fn zero_if_null<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    Ok(Option::<u16>::deserialize(d)?.unwrap_or(0))
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
const fn is_zero(v: &u16) -> bool {
    *v == 0
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
    pub fn save_devices(&self, devices: &[DeviceConfig]) -> io::Result<SaveWait> {
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
    pub fn queue_devices(&self, devices: &[DeviceConfig]) -> io::Result<()> {
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
            signal_poll_secs: 10,
            receiver_type: None,
            update_method: None,
            hdmi_output_over_http: None,
            quick_select_count: None,
            listed_queries: None,
            heos: None,
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
        assert_eq!(0, d.signal_poll_secs);
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
                && !text.contains("hdmi_output_over_http")
                && !text.contains("quick_select_count")
                && !text.contains("listed_queries")
                && !text.contains("heos"),
            "{text}"
        );

        let d = DeviceConfig {
            receiver_type: Some(ReceiverGeneration::AvrX2016),
            update_method: Some(HttpReadMethod::AppCommand),
            hdmi_output_over_http: Some(true),
            quick_select_count: Some(4),
            listed_queries: Some(HttpQueries {
                audyssey: true,
                ..HttpQueries::default()
            }),
            heos: Some(true),
            ..device()
        };
        let text = serde_json::to_string(&d).expect("json");
        assert!(
            text.contains(r#""receiver_type":"avr_x2016""#)
                && text.contains(r#""update_method":"app_command""#)
                && text.contains(r#""hdmi_output_over_http":true"#)
                && text.contains(r#""quick_select_count":4"#)
                && text.contains(r#""audyssey":true"#)
                && text.contains(r#""heos":true"#),
            "{text}"
        );
        assert_eq!(d, serde_json::from_str(&text).expect("json"));
    }

    #[test]
    fn an_unknown_receiver_kind_reads_as_none() {
        let d: DeviceConfig = serde_json::from_str(
            r#"{"id":"A","name":"C","address":"h","receiver_type":"avr_z","update_method":7,"hdmi_output_over_http":"yes","quick_select_count":300,"listed_queries":[1],"heos":"yes"}"#,
        )
        .expect("json");
        assert_eq!(ReceiverKind::default(), d.kind());
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
    fn null_fields_get_the_defaults_like_in_the_python_integration() {
        let text = r#"{"id":"A","name":"C","address":"h","support_sound_mode":null,
            "show_all_inputs":null,"use_telnet":null,"update_audyssey":null,"zone2":null,
            "zone3":null,"volume_step":null,"timeout":null,"is_denon":null,
            "is_dirac_supported":null,"signal_poll_secs":null}"#;
        let d: DeviceConfig = serde_json::from_str(text).expect("json");
        assert!(d.support_sound_mode && d.use_telnet && d.is_denon);
        assert!(!d.show_all_inputs && !d.update_audyssey && !d.zone2 && !d.zone3);
        assert!((d.volume_step - 0.5).abs() < f32::EPSILON);
        assert_eq!((2000, 0), (d.timeout, d.signal_poll_secs));
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
            .save_devices(&devices)
            .expect("save")
            .wait_blocking()
            .expect("written");
        assert_eq!(devices, store.load_devices());

        store.queue_devices(&[]).expect("queue");
        store.writer.flush();
        assert_eq!(Vec::<DeviceConfig>::new(), store.load_devices());
        store.queue_devices(&devices).expect("queue");
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
