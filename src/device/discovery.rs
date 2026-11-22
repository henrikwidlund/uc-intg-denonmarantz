// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Find receivers on the network and connect to them.

use crate::config_store::{self, DeviceConfig, Feature};
use denon_marantz::proto::source::playing_kind;
use denon_marantz::proto::{NetworkAudioCommand, PlayingKind, Power};
use denon_marantz::{
    Ask, Avr, AvrBuilder, Brand, Command, DiscoveredReceiver, DiscoveryConfig, Error, HttpConfig,
    Info, InfoCheck, PageStatus, Query, ReceiverType, SpecFeature, StartOptions, StartReport,
    Started, State, TelnetSync, TunerInfo, UpdateMethod, Zone, discover_all,
};
use log::debug;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};

/// Shortest time limit of one connect try, see [`Stall`].
pub(crate) const STALL_MIN: Duration = Duration::from_millis(200);
/// The time limit of a connect try is this many times the last connect.
const STALL_FACTOR: u32 = 3;
/// A longer time limit is not used: the receiver is slow, and its tries are not started again.
const STALL_MAX: Duration = Duration::from_secs(2);
/// How often a stalled try is started again in one connect.
const STALL_RESTARTS: u8 = 4;
/// How many connect times [`ConnectTimes`] keeps.
const CONNECT_TIMES: usize = 5;
/// Time allowed to open an HTTP connection to the receiver. On the local network it takes a few
/// milliseconds. After wake-up, a lost connect packet is sent again only after 1 s: a new try after
/// this time is faster.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
/// How long the library tries again to open an HTTP connection that cannot be built, for example
/// while the network comes up after wake-up. Such a request was not sent, so this is safe for
/// commands too.
const CONNECT_RETRY: Duration = Duration::from_secs(3);
/// Time between connects while the network is not ready, for example right after wake-up. The fast
/// tries end after [`NETWORK_RETRY_TIMEOUT`].
pub(crate) const NETWORK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// Longest time to retry fast while the network is not ready.
pub const NETWORK_RETRY_TIMEOUT: Duration = Duration::from_secs(3);

/// Discover all receivers. Takes about 3 seconds.
pub async fn discover() -> Result<Vec<DiscoveredReceiver>, Error> {
    #[cfg(test)]
    if let Some(found) = TEST_DISCOVERY.with(|slot| slot.borrow().clone()) {
        return Ok(found);
    }
    discover_all(&DiscoveryConfig::default()).await
}

/// Find the receiver of a configuration in the discovered ones, by its MAC address or serial
/// number.
pub fn find<'a>(
    cfg: &DeviceConfig,
    found: &'a [DiscoveredReceiver],
) -> Option<&'a DiscoveredReceiver> {
    found.iter().find(|r| {
        cfg.same_receiver(r.serial_number.as_deref(), r.mac_address.as_deref()) == Some(true)
    })
}

/// What an earlier connection found out about a receiver. The type saves the type lookup. The brand
/// is used if the device description names none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    /// `None`: the type is looked up.
    pub receiver_type: Option<ReceiverType>,
    /// Used if the device description does not name a brand.
    pub brand: Brand,
}

impl Known {
    pub fn of(cfg: &DeviceConfig) -> Self {
        Self {
            // `Avr` is looked up each time: an error answer at the setup also gives `Avr`, and a
            // probe for `Avr` would keep that wrong type.
            receiver_type: cfg.receiver_type.and_then(|t| match t {
                config_store::ReceiverGeneration::Avr => None,
                config_store::ReceiverGeneration::AvrX => Some(ReceiverType::AvrX),
                config_store::ReceiverGeneration::AvrX2016 => Some(ReceiverType::AvrX2016),
            }),
            brand: if cfg.is_denon {
                Brand::Denon
            } else {
                Brand::Marantz
            },
        }
    }
}

/// Identify a receiver and read its description: a few short HTTP requests. With a known type, only
/// the port of that type is asked.
///
/// If the receiver then reports another type, it is identified again without the known type.
///
/// If the answers do not prove the type, the library reports the oldest type (`Avr`). With a known
/// type that result is not used: it is an error, so the connect is tried again later and the wrong
/// type is not saved. A proven `Avr` is an error too: the stored type is newer.
pub async fn identify(host: &str, known: Option<Known>) -> Result<Avr, Error> {
    let avr = identify_with(host, known).await?;
    let Some(expected) = known.and_then(|k| k.receiver_type) else {
        return Ok(avr);
    };
    if avr.info().receiver_type == expected {
        return Ok(avr);
    }
    debug!(
        "[{host}] receiver type {} is not the stored {}, looking it up again",
        avr.info().receiver_type.name(),
        expected.name()
    );
    let unknown_type = known.map(|k| Known {
        receiver_type: None,
        ..k
    });
    let avr = identify_with(host, unknown_type).await?;
    let info = avr.info();
    if !info.type_proof.is_proven() || info.receiver_type == ReceiverType::Avr {
        return Err(Error::InvalidResponse {
            request: format!(
                "GET http://{host}:{}{}",
                expected.port(),
                denon_marantz::proto::DEVICE_INFO_PATH
            ),
            source: format!(
                "receiver type {} ({:?}), the stored type is {}",
                info.receiver_type.name(),
                info.type_proof,
                expected.name()
            )
            .into(),
        });
    }
    Ok(avr)
}

async fn identify_with(host: &str, known: Option<Known>) -> Result<Avr, Error> {
    builder(host, known).connect().await
}

/// Connect with the values of an earlier connect. It sends no request: the start checks the
/// values, see [`Started::info_check`].
pub async fn connect_known(host: &str, info: Info) -> Result<Avr, Error> {
    builder(host, None).known(info).connect().await
}

/// The builder of a connection. A test can change it, so that the library talks to a fake
/// receiver.
fn builder(host: &str, known: Option<Known>) -> AvrBuilder {
    let mut builder = Avr::builder(host).http_config(HttpConfig {
        connect_timeout: CONNECT_TIMEOUT,
        connect_retry: CONNECT_RETRY,
        ..HttpConfig::default()
    });
    if let Some(known) = known {
        builder = builder.brand(known.brand);
        if let Some(receiver_type) = known.receiver_type {
            builder = builder.probes(&[(receiver_type, receiver_type.port())]);
        }
    }
    #[cfg(test)]
    if let Some(configure) = TEST_BUILDER.with(|slot| slot.borrow().clone()) {
        return configure(host, builder);
    }
    builder
}

/// A function that sets up the builder of the tests on this thread. It gets the host.
#[cfg(test)]
pub type TestBuilder = std::rc::Rc<dyn Fn(&str, AvrBuilder) -> AvrBuilder>;

#[cfg(test)]
thread_local! {
    pub static TEST_BUILDER: std::cell::RefCell<Option<TestBuilder>> = const {
        std::cell::RefCell::new(None)
    };
    /// The receivers that the search finds in the tests on this thread.
    pub static TEST_DISCOVERY: std::cell::RefCell<Option<Vec<DiscoveredReceiver>>> = const {
        std::cell::RefCell::new(None)
    };
}

/// The time limit of the tries of one connect.
///
/// After wake-up, the Wi-Fi link of the Remote can go down for a short time. A lost packet then
/// waits for the TCP resend (1 s or more). A new try is faster.
#[derive(Debug)]
pub struct Stall {
    /// `None`: the tries have no time limit.
    limit: Option<Duration>,
    /// How often a stalled try can still be started again.
    restarts: u8,
}

impl Stall {
    /// The limit is [`STALL_FACTOR`] times `last`, a normal connect time (see
    /// [`ConnectTimes::fastest`]), and at least [`STALL_MIN`]. Without `last`, or if the limit is
    /// over [`STALL_MAX`], there is no limit.
    pub fn new(last: Option<Duration>) -> Self {
        let limit = last
            .map(|last| last.saturating_mul(STALL_FACTOR).max(STALL_MIN))
            .filter(|limit| *limit <= STALL_MAX);
        Self {
            limit,
            restarts: STALL_RESTARTS,
        }
    }
}

/// The times of the last connects. A slow connect after wake-up must not raise the limit of
/// [`Stall`]: the next wake-up would then wait for a lost packet (1 s) instead of starting again.
#[derive(Debug, Default)]
pub struct ConnectTimes {
    times: [Option<Duration>; CONNECT_TIMES],
    next: usize,
}

impl ConnectTimes {
    /// Keep `took`. It replaces the oldest time.
    pub const fn add(&mut self, took: Duration) {
        self.times[self.next] = Some(took);
        self.next = (self.next + 1) % CONNECT_TIMES;
    }

    /// The fastest of the kept times.
    pub fn fastest(&self) -> Option<Duration> {
        self.times.iter().flatten().min().copied()
    }
}

/// Identify a receiver, see [`retry`].
pub async fn identify_retry(
    host: &str,
    known: Option<Known>,
    deadline: Instant,
    stall: &mut Stall,
) -> Result<Avr, Error> {
    let (avr, _) = retry("identify", host, deadline, stall, || identify(host, known)).await?;
    Ok(avr)
}

/// Start the receiver as [`start`] does, see [`retry`]. A connect with known values sends its
/// first request here. Also gives the time of the try that worked.
pub async fn start_retry(
    avr: &Arc<Avr>,
    telnet: bool,
    deadline: Instant,
    stall: &mut Stall,
) -> Result<(Started, Duration), Error> {
    retry("start", &avr.info().host, deadline, stall, || {
        start(avr, telnet)
    })
    .await
}

/// Run `step` until it works. A try that takes longer than the limit of `stall` is started again.
/// After a network error, `step` runs again after a short delay, until `deadline`. Also gives the
/// time of the try that worked. `step_name` names the step in the log.
async fn retry<T, F: Future<Output = Result<T, Error>>>(
    step_name: &str,
    host: &str,
    deadline: Instant,
    stall: &mut Stall,
    mut step: impl FnMut() -> F,
) -> Result<(T, Duration), Error> {
    let begin = Instant::now();
    let mut stalls = 0u8;
    let mut network_errors = 0u16;
    let tries = |ok: bool, last: Duration, stalls: u8, network_errors: u16| {
        debug!(
            "[{host}] {step_name} {} after {} ms: {stalls} stalled, {network_errors} network \
             errors, last try {} ms",
            if ok { "done" } else { "failed" },
            begin.elapsed().as_millis(),
            last.as_millis()
        );
    };
    loop {
        let started = Instant::now();
        let result = match stall.limit.filter(|_| stall.restarts > 0) {
            None => step().await,
            Some(limit) => {
                let Ok(result) = timeout(limit, step()).await else {
                    stall.restarts -= 1;
                    stalls += 1;
                    debug!(
                        "[{host}] {step_name} stalled after {} ms, starting again",
                        limit.as_millis()
                    );
                    continue;
                };
                result
            }
        };
        match result {
            Ok(value) => {
                let took = started.elapsed();
                tries(true, took, stalls, network_errors);
                return Ok((value, took));
            }
            // The library tries again while the connection cannot be built. A connection that
            // closed before the answer is tried again here.
            Err(e) if e.is_network() && Instant::now() + NETWORK_RETRY_INTERVAL < deadline => {
                // One line for the first failure only: wake-up can need many tries.
                if network_errors == 0 {
                    debug!("[{host}] network not ready: {e}");
                }
                network_errors = network_errors.saturating_add(1);
                sleep(NETWORK_RETRY_INTERVAL).await;
            }
            Err(e) => {
                tries(false, started.elapsed(), stalls, network_errors);
                return Err(e);
            }
        }
    }
}

/// Longest wait for the telnet answers of the start, with the extra values.
const TELNET_WAIT: Duration = Duration::from_secs(25);

/// Start the receiver: read the zones, then telnet and the web pages follow in the background.
/// In HTTP mode the settings that otherwise only telnet gives are not read: no entity shows them.
pub async fn start(avr: &Arc<Avr>, telnet: bool) -> Result<Started, Error> {
    let options = StartOptions::new()
        .telnet(telnet)
        .telnet_wait(TELNET_WAIT)
        .settings(telnet);
    // Tests open HEOS only with a fake.
    #[cfg(test)]
    let options = options.heos(super::heos::TEST_HEOS.with(std::cell::Cell::get).is_some());
    avr.start_with(options).await
}

/// First wait between telnet connects that are not fast tries. It doubles after each failure.
const TELNET_RETRY_MIN: Duration = Duration::from_millis(500);
/// Longest wait between telnet connects.
const TELNET_RETRY_MAX: Duration = Duration::from_secs(30);

/// Open telnet after the start could not open it. The library does not try again then. After a
/// network error, it tries again every [`NETWORK_RETRY_INTERVAL`] until `fast_until`, then with a
/// growing wait. Ends when telnet is open.
pub async fn keep_telnet(avr: Arc<Avr>, fast_until: Instant, name: String) {
    let mut wait = TELNET_RETRY_MIN;
    let mut first = true;
    loop {
        let Err(e) = avr.telnet_connect().await else {
            return;
        };
        if first {
            debug!("[{name}] telnet not open, trying again: {e}");
            first = false;
        }
        let pause = if e.is_network() && Instant::now() + NETWORK_RETRY_INTERVAL < fast_until {
            NETWORK_RETRY_INTERVAL
        } else {
            let pause = wait;
            wait = (wait * 2).min(TELNET_RETRY_MAX);
            pause
        };
        sleep(pause).await;
    }
}

/// Time before a web page or a telnet query that failed is tried again.
pub const PAGE_RETRY_DELAY: Duration = Duration::from_secs(4);
/// How often a web page that failed is read again.
const PAGE_RETRIES: usize = 3;

/// The receiver refused the page or did not answer. A page that it does not have is not retried.
const fn failed(status: PageStatus) -> bool {
    matches!(
        status,
        PageStatus::Refused(_) | PageStatus::NoAnswer | PageStatus::Unreadable
    )
}

/// The web pages of the start that the receiver did not give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Missing {
    availability: bool,
    signal: bool,
}

impl Missing {
    fn of(report: &StartReport) -> Self {
        let signal = failed(report.signal);
        report.web_interface.as_ref().map_or(
            Self {
                availability: true,
                signal: true,
            },
            |pages| Self {
                availability: pages.availability.pages.iter().any(|(_, s)| failed(*s)),
                signal: signal || failed(pages.video_signal) || failed(pages.audio_channels),
            },
        )
    }

    const fn any(self) -> bool {
        self.availability || self.signal
    }
}

/// Values that the driver reads after the connect, in addition to the ones of the library. The
/// entities need them. The picture mode is last.
pub(crate) const EXTRA_QUERIES: [Query; 16] = [
    Query::Sleep,
    Query::HdmiOutput,
    Query::SpeakerPreset,
    Query::DigitalCodec,
    Query::AllZoneStereo,
    Query::AudioDelay,
    Query::ReferenceLevel,
    Query::DynamicVolume,
    Query::Bass,
    Query::Treble,
    Query::MultEq,
    Query::BluetoothLevel,
    Query::SoundModeCategory,
    // The toggles of these need the current value.
    Query::Imax,
    Query::TactileChannel,
    Query::PictureMode,
];

/// The values of older receivers. The library tells which ones the receiver has.
pub(crate) const OLDER_QUERIES: [Query; 11] = [
    Query::ProLogicMode,
    Query::SurroundBackMode,
    Query::FrontHeight,
    Query::DynamicCompression,
    Query::AutoFlagDetect,
    Query::DialogLevel,
    Query::AudysseyDsx,
    Query::StageWidth,
    Query::StageHeight,
    Query::HeightGain,
    Query::InstaPrevue,
];

/// What the receiver of a configuration has of the names that are read once per connection.
#[derive(Debug, Clone, Copy)]
pub struct ExtraReads {
    /// The names of the network favorites.
    favorites: bool,
    /// The tuner presets.
    tuner_presets: bool,
}

impl ExtraReads {
    pub fn new(cfg: &DeviceConfig) -> Self {
        Self {
            favorites: cfg.offers(Feature::Spec(SpecFeature::NetworkAudio)),
            tuner_presets: cfg.offers(Feature::Tuner),
        }
    }
}

/// Queue the extra values and the values of older receivers that the receiver may have. It does
/// not wait for the answers.
pub async fn read_extra_values(avr: &Avr) {
    let info = avr.info();
    let asks: Vec<Ask<'_>> = EXTRA_QUERIES
        .into_iter()
        .chain(OLDER_QUERIES)
        .filter(|&query| query.text(info.brand).is_some() && info.offers(query))
        .map(Ask::from)
        .collect();
    // An empty ask clears the filter of repeated events in the library.
    if asks.is_empty() {
        return;
    }
    if let Err(e) = avr.telnet_ask(&asks).await {
        debug!("extra values not read: {e}");
    }
}

/// Pass other receiver values than the stored ones to `changed` as soon as the start has checked
/// them. Then wait for the end of the start and, with telnet, for the answers of its telnet
/// queries, and run [`read_missing_pages`]. Runs in the background: commands do not wait for it.
pub async fn after_start(
    avr: Arc<Avr>,
    mut started: Started,
    telnet: bool,
    name: String,
    delay: Duration,
    changed: impl FnOnce(Info),
    synced: impl FnOnce(),
) {
    // The extra values are queued before the wait: it covers the queries that wait at the call.
    if telnet {
        read_extra_values(&avr).await;
    }
    match started.info_check().await {
        InfoCheck::Changed(info) => changed(Info::clone(info)),
        InfoCheck::Failed(e) => debug!("[{name}] stored receiver values not checked: {e}"),
        _ => {}
    }
    // With HTTP only, a page read could delay a command: the pages are not read again.
    if !telnet {
        return;
    }
    let report = started.all_synced().await;
    if report.telnet_sync == TelnetSync::NoAnswer {
        debug!("[{name}] no answer to the telnet queries of the start");
    }
    let missing = Missing::of(report);
    let unanswered = report.telnet_not_answered.clone();
    // `Started` holds the receiver, so the reads would not stop after a reconnect.
    drop(started);
    read_missing_pages(avr, missing, unanswered, name, delay, synced).await;
}

/// The receiver sometimes answers a web page with an error (HTTP status 500) and does not answer
/// every telnet query when it is busy. The library asks once more at once. This reads the pages
/// again after a pause, and asks the `unanswered` telnet queries once more, so that their values
/// do not stay empty until the next connect. Call it when the answers of the start are in. It
/// calls `synced` first.
async fn read_missing_pages(
    avr: Arc<Avr>,
    mut missing: Missing,
    unanswered: Vec<Cow<'static, str>>,
    name: String,
    delay: Duration,
    synced: impl FnOnce(),
) {
    synced();
    for attempt in 1..=PAGE_RETRIES {
        if !missing.any() {
            break;
        }
        sleep(delay).await;
        // Nobody else holds the receiver after a reconnect: stop.
        if Arc::strong_count(&avr) <= 1 {
            return;
        }
        debug!("[{name}] reading web pages again, try {attempt}");
        if missing.availability {
            match avr.update_settings_availability().await {
                Ok(read) => missing.availability = read.pages.iter().any(|(_, s)| failed(*s)),
                Err(e) => debug!("[{name}] settings pages not read: {e}"),
            }
        }
        if missing.signal {
            match avr.refresh_signal_pages().await {
                Ok(()) => missing.signal = false,
                Err(e) => debug!("[{name}] signal pages not read: {e}"),
            }
        }
    }
    // Once: the receiver can drop a query now and then, and one more try is enough.
    if unanswered.is_empty() {
        return;
    }
    sleep(delay).await;
    if Arc::strong_count(&avr) <= 1 {
        return;
    }
    debug!(
        "[{name}] asking {} telnet queries again: {unanswered:?}",
        unanswered.len()
    );
    let asks: Vec<Ask<'_>> = unanswered.iter().map(|query| Ask::Raw(query)).collect();
    if let Err(e) = avr.telnet_ask(&asks).await {
        debug!("[{name}] telnet queries not asked again: {e}");
    }
}

/// What the setup needs to know about a receiver.
#[derive(Debug, Clone)]
pub struct Identified {
    /// The MAC address, or the serial number on old receivers that do not give a MAC address. It
    /// is the id of the receiver.
    pub id: String,
    pub id_kind: config_store::IdKind,
    pub serial: Option<String>,
    /// 12 uppercase hexadecimal digits.
    pub mac: Option<String>,
    pub name: String,
    pub model: String,
    pub is_denon: bool,
    pub is_dirac_supported: bool,
    pub support_sound_mode: bool,
    pub kind: config_store::ReceiverKind,
    /// For the next connect, see [`DeviceConfig::info`].
    pub info: Info,
}

impl Identified {
    /// Fails with the reason if the answers do not prove the receiver type, if the description
    /// was not read, or if the receiver gives no MAC address and is not an old receiver with a
    /// serial number.
    pub fn new(avr: &Avr) -> Result<Self, &'static str> {
        let info = avr.info();
        if !info.type_proof.is_proven() {
            return Err("the answers do not prove the receiver type");
        }
        // The library reads no description without a manufacturer. The name, model and brand
        // come from the description and are not read again.
        if info.manufacturer.is_none() {
            return Err("the receiver did not send its description");
        }
        let serial = info.serial_number.clone();
        let mac = mac_address(info);
        let (id, id_kind) = match (&mac, &serial) {
            (Some(mac), _) => (mac.clone(), config_store::IdKind::Mac),
            (None, Some(serial)) if info.receiver_type == ReceiverType::Avr => {
                (serial.clone(), config_store::IdKind::Serial)
            }
            (None, _) if info.receiver_type != ReceiverType::Avr => {
                return Err("the receiver gave no MAC address");
            }
            _ => return Err("the receiver gave no serial number"),
        };
        Ok(Self {
            id,
            id_kind,
            serial,
            mac,
            name: if info.friendly_name.is_empty() {
                info.model_name.clone().unwrap_or_default()
            } else {
                info.friendly_name.clone()
            },
            model: info.model_name.clone().unwrap_or_default(),
            is_denon: info.brand != Brand::Marantz,
            is_dirac_supported: info.dirac_live,
            // Receivers that are not AVR-X have no sound mode list.
            support_sound_mode: info.receiver_type != ReceiverType::Avr,
            kind: receiver_kind(info),
            info: info.clone(),
        })
    }
}

/// The MAC address as 12 uppercase hexadecimal digits. `None` if the receiver does not give a
/// valid one.
pub fn mac_address(info: &Info) -> Option<String> {
    info.mac_address.as_deref().and_then(config_store::mac_text)
}

/// The telnet port of the receivers.
pub const TELNET_PORT: u16 = 23;

/// Time to wait for the answer to one query of the tuner check.
const TUNER_ANSWER_WAIT: Duration = Duration::from_millis(1500);

/// Ask the receiver for its tuner band, then its frequency. An answer means a tuner. `None` if
/// it is not known: telnet cannot be opened, or the receiver is not on.
///
/// `Deviceinfo.xml` cannot tell: a receiver without a tuner can list an analog input named
/// `TUNER`, and the tuner depends on the sales region. A receiver without a tuner does not answer
/// (seen on an AVC-A10H). No capture shows that a receiver with a tuner answers while another
/// source plays, so the result is only a default for the setup.
pub async fn probe_tuner(host: &str, port: u16) -> Option<bool> {
    use tokio::io::AsyncWriteExt;
    let mut stream = timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    .ok()?
    .ok()?;
    let mut lines = Lines::default();
    // A receiver in standby may not answer the tuner queries.
    stream.write_all(b"PW?\r").await.ok()?;
    let on = lines
        .wait(&mut stream, |line| match line {
            b"PWON" => Some(true),
            b"PWSTANDBY" => Some(false),
            _ => None,
        })
        .await?;
    if !on {
        return None;
    }
    for query in [&b"TMAN?\r"[..], b"TFAN?\r"] {
        stream.write_all(query).await.ok()?;
        let tuner =
            |line: &[u8]| (line.starts_with(b"TM") || line.starts_with(b"TF")).then_some(());
        if lines.wait(&mut stream, tuner).await.is_some() {
            return Some(true);
        }
    }
    Some(false)
}

/// Reads telnet lines. A line can be split over two reads.
struct Lines {
    buffer: [u8; 1024],
    line: Vec<u8>,
}

impl Default for Lines {
    fn default() -> Self {
        Self {
            buffer: [0; 1024],
            line: Vec::with_capacity(64),
        }
    }
}

impl Lines {
    /// The first line for which `pick` gives a value, within `TUNER_ANSWER_WAIT`. `None` if no
    /// line fits, or if the connection ends.
    async fn wait<T>(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        mut pick: impl FnMut(&[u8]) -> Option<T>,
    ) -> Option<T> {
        use tokio::io::AsyncReadExt;
        let end = Instant::now() + TUNER_ANSWER_WAIT;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Ok(Ok(count)) = timeout(left, stream.read(&mut self.buffer)).await else {
                return None;
            };
            if count == 0 {
                return None;
            }
            for &byte in &self.buffer[..count] {
                if byte != b'\r' {
                    // A long line is not an answer of the check: only its start is kept.
                    if self.line.len() < 64 {
                        self.line.push(byte);
                    }
                    continue;
                }
                let found = pick(&self.line);
                self.line.clear();
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }
}

/// The names that the selects show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Names {
    TunerPresets,
    NetworkFavorites,
}

impl Names {
    /// The bit of the names in a set of `u8`.
    pub const fn bit(self) -> u8 {
        match self {
            Self::TunerPresets => 1,
            Self::NetworkFavorites => 2,
        }
    }

    /// The names that the main zone uses now: the tuner presets while the tuner plays, the network
    /// favorites while network audio plays. None while the main zone is off: a receiver in standby
    /// may not answer.
    pub fn in_use(reads: ExtraReads, state: &State) -> Option<Self> {
        if state.zone(Zone::Main).power != Some(Power::On) {
            return None;
        }
        match main_playing_kind(state)? {
            PlayingKind::Tuner if reads.tuner_presets => Some(Self::TunerPresets),
            PlayingKind::NetAudio if reads.favorites => Some(Self::NetworkFavorites),
            _ => None,
        }
    }

    /// The names that a raw command saves (`TPANMEM01`, `NSC01`).
    pub fn saved_by(reads: ExtraReads, text: &str) -> Option<Self> {
        let text = text.trim_start();
        if reads.tuner_presets && text.starts_with("TPANMEM") {
            Some(Self::TunerPresets)
        } else if reads.favorites && text.starts_with("NSC") {
            Some(Self::NetworkFavorites)
        } else {
            None
        }
    }

    /// Ask the receiver for the names. It does not wait for the answers.
    pub async fn read(self, avr: &Avr) {
        let result = match self {
            Self::TunerPresets => avr.tuner().request_presets().await,
            Self::NetworkFavorites => {
                let names = Command::NetworkAudio(NetworkAudioCommand::PresetNames);
                avr.send(Zone::Main, names).await
            }
        };
        if let Err(e) = result {
            debug!("{self:?} not read: {e}");
        }
    }

    /// The stored names with the ones that the receiver reported. `None` if nothing changed. It
    /// compares first, with one text buffer: while tuning, the presets do not change.
    pub fn merged(self, cfg: &DeviceConfig, state: &State) -> Option<Vec<String>> {
        let device = state.device();
        let (stored, count) = match self {
            Self::TunerPresets => (&cfg.tuner_preset_names, device.tuner.presets.len()),
            Self::NetworkFavorites => (
                &cfg.network_favorite_names,
                device.network_preset_names.len(),
            ),
        };
        // Write the reported text of entry `i` into `text`. False if it was not reported.
        let reported = |i: usize, text: &mut String| -> bool {
            text.clear();
            match self {
                // A preset that the receiver reported has a frequency.
                Self::TunerPresets => {
                    let preset = &device.tuner.presets[i];
                    let name = preset
                        .name
                        .as_deref()
                        .map(str::trim)
                        .filter(|n| !n.is_empty());
                    match (name, preset.frequency) {
                        (Some(name), _) => text.push_str(name),
                        (None, Some(frequency)) => {
                            use std::fmt::Write;
                            let _ = write!(text, "{frequency}");
                        }
                        (None, None) => return false,
                    }
                }
                // The answers come in order: the names up to the last one are reported.
                Self::NetworkFavorites => {
                    text.push_str(
                        device.network_preset_names[i]
                            .as_deref()
                            .map_or("", str::trim),
                    );
                }
            }
            true
        };
        let mut text = String::new();
        let changed = (0..count).any(|i| {
            reported(i, &mut text) && stored.get(i).map(String::as_str) != Some(text.as_str())
        });
        if !changed {
            return None;
        }
        let mut merged = stored.clone();
        if merged.len() < count {
            merged.resize(count, String::new());
        }
        for (i, slot) in merged.iter_mut().enumerate().take(count) {
            if reported(i, &mut text) {
                slot.clone_from(&text);
            }
        }
        Some(merged)
    }
}

/// What the source of the main zone plays, from its name or the code of a renamed source.
fn main_playing_kind(state: &State) -> Option<PlayingKind> {
    let source = state.zone(Zone::Main).source.as_deref()?;
    playing_kind(source).or_else(|| {
        state
            .device()
            .sources
            .as_slice()
            .iter()
            .find(|info| info.name.as_deref() == Some(source))
            .and_then(|info| playing_kind(&info.code))
    })
}

/// What the configuration keeps of the receiver.
pub fn receiver_kind(info: &Info) -> config_store::ReceiverKind {
    config_store::ReceiverKind {
        receiver_type: receiver_type(info),
        update_method: update_method(info),
        hdmi_output_over_http: Some(info.hdmi_output_over_http()),
        listed_queries: info.queries.as_ref().map(|q| config_store::HttpQueries {
            tone_control: q.tone_control,
            audyssey: q.audyssey,
            video_info: q.video_info,
            audio_info: q.audio_info,
            input_signal: q.input_signal,
            active_speaker: q.active_speaker,
            output_settings: q.output_settings,
        }),
        comm_api_version: info.comm_api_version.as_deref().map_or(
            Some(config_store::CommApiVersion::MISSING),
            config_store::CommApiVersion::parse,
        ),
        has_tuner: match info.tuner {
            TunerInfo::Present(_) => Some(true),
            TunerInfo::Absent => Some(false),
            _ => None,
        },
    }
}

/// The receiver generation for the configuration. `None` for a generation that this version does
/// not know.
const fn receiver_type(info: &Info) -> Option<config_store::ReceiverGeneration> {
    match info.receiver_type {
        ReceiverType::Avr => Some(config_store::ReceiverGeneration::Avr),
        ReceiverType::AvrX => Some(config_store::ReceiverGeneration::AvrX),
        ReceiverType::AvrX2016 => Some(config_store::ReceiverGeneration::AvrX2016),
        _ => None,
    }
}

/// How the receiver is read over HTTP, for the configuration. `None` for a method that this
/// version does not know.
const fn update_method(info: &Info) -> Option<config_store::HttpReadMethod> {
    match info.update_method {
        UpdateMethod::AppCommand => Some(config_store::HttpReadMethod::AppCommand),
        UpdateMethod::StatusPages => Some(config_store::HttpReadMethod::StatusPages),
        _ => None,
    }
}

#[cfg(test)]
mod page_tests {
    use super::*;
    use denon_marantz::proto::SettingsGroup;
    use denon_marantz::{HeosStart, PageResults, StartPages, TelnetStart};

    fn report(availability: PageStatus, video: PageStatus, channels: PageStatus) -> StartReport {
        StartReport {
            telnet: TelnetStart::Unknown,
            signal: PageStatus::Read,
            telnet_sync: TelnetSync::NotWaited,
            heos: HeosStart::Disabled,
            telnet_not_answered: Vec::new(),
            web_interface: Ok(StartPages {
                availability: PageResults {
                    pages: vec![
                        (SettingsGroup::Audio, PageStatus::Read),
                        (SettingsGroup::Option, availability),
                    ],
                },
                video_signal: video,
                audio_channels: channels,
            }),
        }
    }

    #[test]
    fn a_refused_page_is_read_again() {
        let ok = PageStatus::Read;
        let all = Missing::of(&report(ok, ok, ok));
        assert!(!all.any());
        let option = Missing::of(&report(PageStatus::Refused(500), ok, ok));
        assert!(option.availability && !option.signal);
        let video = Missing::of(&report(ok, PageStatus::NoAnswer, ok));
        assert!(!video.availability && video.signal);
    }

    #[test]
    fn a_signal_read_that_failed_is_read_again() {
        let ok = PageStatus::Read;
        let mut report = report(ok, ok, ok);
        report.signal = PageStatus::Unreadable;
        let missing = Missing::of(&report);
        assert!(missing.signal && !missing.availability);
    }

    #[test]
    fn a_missing_page_is_not_read_again() {
        let ok = PageStatus::Read;
        let missing = Missing::of(&report(PageStatus::NotFound, ok, PageStatus::NotFound));
        assert!(!missing.any());
    }

    #[test]
    fn all_pages_are_read_again_if_the_web_interface_failed() {
        let report = StartReport {
            telnet: TelnetStart::Unknown,
            signal: PageStatus::NoAnswer,
            telnet_sync: TelnetSync::NotWaited,
            heos: HeosStart::Disabled,
            telnet_not_answered: Vec::new(),
            web_interface: Err(Error::Stopped),
        };
        assert_eq!(
            Missing {
                availability: true,
                signal: true
            },
            Missing::of(&report)
        );
    }

    use crate::test_support::{Fake, FakeWeb};

    /// The page of the settings availability that the library asks for first.
    const AUDIO_PAGE: &str = "/ajax/audio/get_config?type=1";

    async fn started_with_web_status(status: u16) -> (FakeWeb, Arc<Avr>, Started) {
        let fake = Fake::start(true).await;
        let web = FakeWeb::start(status).await;
        let avr = Arc::new(fake.avr_with_web(&web).await);
        let started = start(&avr, false).await.expect("start");
        // The fake receiver must stay up: it is dropped with the server task at the end.
        std::mem::forget(fake);
        (web, avr, started)
    }

    #[tokio::test]
    async fn a_refused_page_is_asked_for_again_three_times() {
        let (web, avr, mut started) = started_with_web_status(500).await;
        // The actor holds the receiver while it runs: the reads stop if nobody else holds it.
        let held = avr.clone();
        let missing = Missing::of(started.basic_synced().await);
        drop(started);
        read_missing_pages(
            avr,
            missing,
            Vec::new(),
            "AVR".into(),
            Duration::from_millis(10),
            || (),
        )
        .await;
        drop(held);
        // Two requests at the start and two for each of the three tries: the library asks twice.
        assert_eq!(2 + 3 * 2, web.requests(AUDIO_PAGE));
    }

    #[tokio::test]
    async fn the_reads_stop_when_nobody_holds_the_receiver() {
        let (web, avr, mut started) = started_with_web_status(500).await;
        let missing = Missing::of(started.basic_synced().await);
        drop(started);
        read_missing_pages(
            avr,
            missing,
            Vec::new(),
            "AVR".into(),
            Duration::from_millis(10),
            || (),
        )
        .await;
        assert_eq!(2, web.requests(AUDIO_PAGE));
    }

    #[tokio::test]
    async fn a_missing_page_is_not_asked_for_again() {
        let (web, avr, mut started) = started_with_web_status(404).await;
        let held = avr.clone();
        let missing = Missing::of(started.basic_synced().await);
        drop(started);
        read_missing_pages(
            avr,
            missing,
            Vec::new(),
            "AVR".into(),
            Duration::from_millis(10),
            || (),
        )
        .await;
        drop(held);
        assert_eq!(1, web.requests(AUDIO_PAGE));
    }
}

#[cfg(test)]
mod tuner_tests {
    use super::*;
    use crate::test_support::FakeTelnet;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn an_answer_to_the_tuner_query_means_a_tuner() {
        let telnet = FakeTelnet::start(|line| match line {
            "PW?" => vec!["PWON".to_owned()],
            "TFAN?" => vec!["TFAN010110".to_owned()],
            _ => Vec::new(),
        })
        .await;
        assert_eq!(Some(true), probe_tuner("127.0.0.1", telnet.port).await);
    }

    #[tokio::test]
    async fn no_answer_means_no_tuner() {
        let telnet = FakeTelnet::start(|line| match line {
            "PW?" => vec!["PWON".to_owned()],
            _ => Vec::new(),
        })
        .await;
        assert_eq!(Some(false), probe_tuner("127.0.0.1", telnet.port).await);
    }

    #[tokio::test]
    async fn it_is_not_known_in_standby_or_with_a_closed_port() {
        let telnet = FakeTelnet::start(|line| match line {
            "PW?" => vec!["PWSTANDBY".to_owned()],
            "TMAN?" => vec!["TMANFM".to_owned()],
            _ => Vec::new(),
        })
        .await;
        assert_eq!(None, probe_tuner("127.0.0.1", telnet.port).await);
        assert_eq!(0, telnet.count("TMAN?"));
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("address")
            .port();
        assert_eq!(None, probe_tuner("127.0.0.1", closed).await);
    }

    #[tokio::test]
    async fn an_answer_in_two_parts_is_found() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 64];
            let _ = stream.read(&mut buffer).await;
            stream.write_all(b"PWO").await.expect("write");
            sleep(Duration::from_millis(50)).await;
            stream.write_all(b"N\r").await.expect("write");
            let _ = stream.read(&mut buffer).await;
            stream.write_all(b"MVMAX 98\rT").await.expect("write");
            sleep(Duration::from_millis(50)).await;
            stream.write_all(b"MANFM\r").await.expect("write");
            sleep(Duration::from_secs(5)).await;
        });
        assert_eq!(Some(true), probe_tuner("127.0.0.1", port).await);
    }
}
