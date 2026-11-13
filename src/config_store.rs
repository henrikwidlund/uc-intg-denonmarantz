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
    /// Serial number of the receiver. It is also the device key of the entity ids.
    pub id: String,
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
}

impl DeviceConfig {
    /// Key for the entity ids.
    pub fn key(&self) -> &str {
        &self.id
    }

    pub const fn has_dirac(&self) -> bool {
        matches!(self.is_dirac_supported, Some(true))
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
