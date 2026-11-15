// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Find receivers on the network and connect to them.

use crate::config_store::{self, DeviceConfig};
use denon_marantz::{
    Ask, Avr, AvrBuilder, DiscoveredReceiver, DiscoveryConfig, Error, PageStatus, Query,
    QueryGroup, ReceiverType, StartOptions, StartReport, Started, UpdateMethod, discover_all,
};
use log::debug;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};

/// A connect step without answer in this time is started again once.
///
/// After wake-up, the Wi-Fi link of the Remote can go down for a short time. Lost packets then
/// wait for the TCP resend (1 s or more). A connect step takes much less.
const STALL_TIMEOUT: Duration = Duration::from_millis(500);
/// Time between connects while the network is not ready, for example right after wake-up. It
/// grows, so that a receiver that is off does not get a connect attempt every 50 ms.
const NETWORK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const NETWORK_RETRY_INTERVAL_MAX: Duration = Duration::from_millis(250);
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

/// Find the receiver of a configuration in the discovered ones. The serial number is the id.
pub fn find<'a>(
    cfg: &DeviceConfig,
    found: &'a [DiscoveredReceiver],
) -> Option<&'a DiscoveredReceiver> {
    found
        .iter()
        .find(|r| r.serial_number.as_deref() == Some(cfg.id.as_str()))
}

/// Identify a receiver. One short HTTP step. The step is started again once if it stalls.
pub async fn identify(host: &str) -> Result<Avr, Error> {
    for attempt in 0..2 {
        match timeout(STALL_TIMEOUT, builder(host).connect()).await {
            Ok(result) => return result,
            Err(_) if attempt == 0 => debug!("[{host}] connect step stalled, starting again"),
            Err(_) => {}
        }
    }
    builder(host).connect().await
}

/// The builder of a connection. A test can change it, so that the library talks to a fake
/// receiver.
fn builder(host: &str) -> AvrBuilder {
    #[cfg(test)]
    if let Some(configure) = TEST_BUILDER.with(|slot| slot.borrow().clone()) {
        return configure(host, Avr::builder(host));
    }
    Avr::builder(host)
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

/// Identify a receiver and retry network errors fast until `deadline`.
pub async fn identify_retry(host: &str, deadline: Instant) -> Result<Avr, Error> {
    let mut delay = NETWORK_RETRY_INTERVAL;
    let mut first = true;
    loop {
        match identify(host).await {
            Err(e) if e.is_unreachable() && Instant::now() + delay < deadline => {
                // One line for the first failure only: wake-up can need many tries.
                if first {
                    debug!("[{host}] network not ready: {e}");
                    first = false;
                }
                sleep(delay).await;
                delay = (delay + NETWORK_RETRY_INTERVAL).min(NETWORK_RETRY_INTERVAL_MAX);
            }
            result => return result,
        }
    }
}

/// Longest wait for the telnet answers of the start, with the extra values.
const TELNET_WAIT: Duration = Duration::from_secs(25);

/// Start the receiver: read the zones, then telnet and the web pages follow in the background.
pub async fn start(avr: &Arc<Avr>, telnet: bool) -> Result<Started, Error> {
    let options = StartOptions::new().telnet(telnet).telnet_wait(TELNET_WAIT);
    avr.start_with(options).await
}

/// Time before a web page that failed is read again.
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
const EXTRA_QUERIES: [Query; 12] = [
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
    Query::PictureMode,
];

/// Queue the extra values. It does not wait for the answers.
pub async fn read_extra_values(avr: &Avr) {
    let asks = EXTRA_QUERIES.map(Ask::from);
    if let Err(e) = avr.telnet_ask(&asks).await {
        debug!("extra values not read: {e}");
    }
}

/// How often the toggle queries are sent again when a value is missing. Once: the receiver can
/// drop a query now and then, and one more try is enough.
const TOGGLE_RETRIES: usize = 1;

/// A switch of the entities has no value. A receiver that is busy does not answer every query.
fn toggles_missing(avr: &Avr) -> bool {
    let state = avr.state();
    let state = state.borrow();
    let device = state.device();
    device.tone_control.is_none()
        || device.dynamic_eq.is_none()
        || device.bluetooth_transmitter.is_none()
        || device.triggers.iter().any(Option::is_none)
}

/// The receiver sometimes answers a web page with an error (HTTP status 500) and does not answer
/// every telnet query when it is busy. The library asks once more at once. This reads the pages
/// and the toggles again after a pause, so that their values do not stay empty until the next
/// connect. It starts when the answers of the start are in. Runs in the background: commands do
/// not wait for it.
pub async fn read_missing_pages(avr: Arc<Avr>, started: Started, name: String, delay: Duration) {
    // The extra values are queued before the wait: it covers the queries that wait at the call.
    read_extra_values(&avr).await;
    // The answers of the telnet queries of the start are in the state after this.
    let mut missing = Missing::of(&started.all_synced().await);
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
            match avr.refresh_signal().await {
                Ok(()) => missing.signal = false,
                Err(e) => debug!("[{name}] signal pages not read: {e}"),
            }
        }
    }
    for attempt in 1..=TOGGLE_RETRIES {
        if Arc::strong_count(&avr) <= 1 || !toggles_missing(&avr) {
            return;
        }
        sleep(delay).await;
        debug!("[{name}] asking for the toggles again, try {attempt}");
        if let Err(e) = avr.telnet_refresh_group(QueryGroup::Toggles).await {
            debug!("[{name}] toggles not read: {e}");
        }
    }
}

/// What the setup needs to know about a receiver.
#[derive(Debug, Clone)]
pub struct Identified {
    /// Serial number. It is the id of the receiver.
    pub id: String,
    pub name: String,
    pub model: String,
    pub is_denon: bool,
    pub is_dirac_supported: bool,
    pub support_sound_mode: bool,
    pub kind: config_store::ReceiverKind,
}

impl Identified {
    pub fn new(avr: &Avr) -> Option<Self> {
        let info = avr.info();
        Some(Self {
            id: info.serial_number.clone().filter(|s| !s.is_empty())?,
            name: if info.friendly_name.is_empty() {
                info.model_name.clone().unwrap_or_default()
            } else {
                info.friendly_name.clone()
            },
            model: info.model_name.clone().unwrap_or_default(),
            is_denon: info.brand != denon_marantz::Brand::Marantz,
            is_dirac_supported: info.dirac_live,
            // Receivers that are not AVR-X have no sound mode list.
            support_sound_mode: info.receiver_type != ReceiverType::Avr,
            kind: receiver_kind(info),
        })
    }
}

/// What the configuration keeps of the receiver.
pub const fn receiver_kind(info: &denon_marantz::Info) -> config_store::ReceiverKind {
    config_store::ReceiverKind {
        receiver_type: receiver_type(info),
        update_method: update_method(info),
        hdmi_output_over_http: Some(info.hdmi_output_over_http),
        quick_select_count: info.quick_select_count,
        listed_queries: match &info.queries {
            Some(q) => Some(config_store::HttpQueries {
                tone_control: q.tone_control,
                audyssey: q.audyssey,
                video_info: q.video_info,
                audio_info: q.audio_info,
                input_signal: q.input_signal,
                active_speaker: q.active_speaker,
                output_settings: q.output_settings,
            }),
            None => None,
        },
    }
}

/// The receiver generation for the configuration. `None` for a generation that this version does
/// not know.
const fn receiver_type(info: &denon_marantz::Info) -> Option<config_store::ReceiverGeneration> {
    match info.receiver_type {
        ReceiverType::Avr => Some(config_store::ReceiverGeneration::Avr),
        ReceiverType::AvrX => Some(config_store::ReceiverGeneration::AvrX),
        ReceiverType::AvrX2016 => Some(config_store::ReceiverGeneration::AvrX2016),
        _ => None,
    }
}

/// How the receiver is read over HTTP, for the configuration. `None` for a method that this
/// version does not know.
const fn update_method(info: &denon_marantz::Info) -> Option<config_store::HttpReadMethod> {
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
    use denon_marantz::{PageResults, StartPages, TelnetStart, TelnetSync};

    fn report(availability: PageStatus, video: PageStatus, channels: PageStatus) -> StartReport {
        StartReport {
            telnet: TelnetStart::Unknown,
            signal: PageStatus::Read,
            telnet_sync: TelnetSync::NotWaited,
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
        let (web, avr, started) = started_with_web_status(500).await;
        // The actor holds the receiver while it runs: the reads stop if nobody else holds it.
        let held = avr.clone();
        read_missing_pages(avr, started, "AVR".into(), Duration::from_millis(10)).await;
        drop(held);
        // Two requests at the start and two for each of the three tries: the library asks twice.
        assert_eq!(2 + 3 * 2, web.requests(AUDIO_PAGE));
    }

    #[tokio::test]
    async fn the_reads_stop_when_nobody_holds_the_receiver() {
        let (web, avr, started) = started_with_web_status(500).await;
        read_missing_pages(avr, started, "AVR".into(), Duration::from_millis(10)).await;
        assert_eq!(2, web.requests(AUDIO_PAGE));
    }

    #[tokio::test]
    async fn a_missing_page_is_not_asked_for_again() {
        let (web, avr, started) = started_with_web_status(404).await;
        let held = avr.clone();
        read_missing_pages(avr, started, "AVR".into(), Duration::from_millis(10)).await;
        drop(held);
        assert_eq!(1, web.requests(AUDIO_PAGE));
    }
}
