// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! A fake receiver for the tests: a small HTTP server. It answers like a receiver of the AVR-X
//! family and records the commands that it gets.

use denon_marantz::{Avr, HttpConfig, Pacing, ReceiverType, TelnetConfig};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

const DESCRIPTION: &[u8] = include_bytes!("test_data/description.xml");
const INFO: &[u8] =
    b"<Device_Info><CommApiVers>0301</CommApiVers><ModelName>AVC-X3700H</ModelName>\
    <DeviceZones>2</DeviceZones><MacAddress>0123456789AB</MacAddress></Device_Info>";
/// The device information without the MAC address.
const INFO_WITHOUT_MAC: &[u8] =
    b"<Device_Info><CommApiVers>0301</CommApiVers><ModelName>AVC-X3700H</ModelName>\
    <DeviceZones>2</DeviceZones></Device_Info>";
/// The answer of old receivers that have no device information, for example the AVR-3311CI.
const OLD_INFO: &[u8] = b"<html><head><title>Document Error: Data follows</title></head>\
    <body><h2>Access Error: Data follows</h2><p>Form Deviceinfo.xml is not defined</p></body></html>";
/// The answer of `AppCommand0300.xml` to the queries of the library, in its order.
const APPCOMMAND0300: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\" ?><rx>\
    <cmd><name>GetAudyssey</name><list>\
    <param name=\"dynamiceq\" control=\"0\"></param>\
    <param name=\"reflevoffset\" control=\"0\"></param>\
    <param name=\"dynamicvol\" control=\"0\"></param>\
    <param name=\"multeq\" control=\"0\"></param></list></cmd>\
    <cmd><name>GetVideoInfo</name><list></list></cmd>\
    <cmd><name>GetAudioInfo</name><list></list></cmd>\
    <cmd><name>GetInputSignal</name><list></list></cmd>\
    <cmd><name>GetActiveSpeaker</name><list></list></cmd></rx>";
/// The path of the commands.
const COMMAND: &str = "/goform/formiPhoneAppDirect.xml?";

/// The names of the queries of the update request, in the order of the items of the answer.
const UPDATE_NAMES: [&str; 13] = [
    "GetAllZonePowerStatus",
    "GetAutoStandby",
    "GetDimmer",
    "GetECO",
    "GetAllZoneSource",
    "GetAllZoneVolume",
    "GetAllZoneMuteStatus",
    "GetSurroundModeStatus",
    "GetRenameSource",
    "GetDeletedSource",
    "GetZoneName",
    "GetFriendlyName",
    // Tone control is off, so a bass or treble command turns it on first.
    "GetToneControl",
];

/// The answer to an `AppCommand` request: the items for the queries that the request has, in the
/// order of the request.
fn answer_for(request: &[u8], power_on: bool, main_source: &str) -> Vec<u8> {
    let whole = String::from_utf8(update_answer(power_on, main_source)).expect("text");
    let request = String::from_utf8_lossy(request);
    let mut asked: Vec<(usize, usize)> = UPDATE_NAMES
        .iter()
        .enumerate()
        .filter_map(|(index, name)| request.find(&format!(">{name}<")).map(|at| (at, index)))
        .collect();
    asked.sort_unstable();
    if asked.is_empty() {
        return whole.into_bytes();
    }
    let body = &whole["<rx>".len()..whole.len() - "</rx>".len()];
    let items: Vec<&str> = body.split_inclusive("</cmd>").collect();
    let mut out = String::from("<rx>");
    for (_, index) in asked {
        out.push_str(items[index].trim());
    }
    out.push_str("</rx>");
    out.into_bytes()
}

/// The answer to the update request: the main zone is on if `power_on`.
fn update_answer(power_on: bool, main_source: &str) -> Vec<u8> {
    let power = if power_on { "ON" } else { "OFF" };
    format!(
        "<rx>\
        <cmd><zone1>{power}</zone1><zone2>OFF</zone2></cmd>\
        <cmd><list><listvalue><zone>Main</zone><value>60</value></listvalue>\
            <listvalue><zone>Zone2</zone><value>0</value></listvalue></list></cmd>\
        <cmd><value>2</value></cmd>\
        <cmd><mode>1</mode></cmd>\
        <cmd><zone1><source>{main_source}</source></zone1><zone2><source>CD</source></zone2></cmd>\
        <cmd><zone1><volume>-36.5</volume><limit>-10</limit></zone1>\
            <zone2><volume>--</volume><limit>OFF</limit></zone2></cmd>\
        <cmd><zone1>off</zone1><zone2>ON</zone2></cmd>\
        <cmd><surround>DOLBY  ATMOS</surround></cmd>\
        <cmd><functionrename>\
            <list><name>Media Player</name><rename>Apple TV </rename></list>\
            <list><name>CD</name><rename></rename></list></functionrename></cmd>\
        <cmd><functiondelete>\
            <list><name>Media Player</name><FuncName>Media Player</FuncName><use>1</use></list>\
            <list><name>CD</name><FuncName>CD</FuncName><use>0</use></list></functiondelete></cmd>\
        <cmd><zone1>MAIN ZONE </zone1><zone2>Kitchen</zone2></cmd>\
        <cmd><friendlyname>Denon AVR</friendlyname></cmd>\
        <cmd><status>0</status><adjust>0</adjust><basslevel>0dB</basslevel>\
            <bassvalue>6</bassvalue><treblelevel>0dB</treblelevel><treblevalue>6</treblevalue></cmd>\
        </rx>"
    )
    .into_bytes()
}

/// A fake receiver.
pub struct Fake {
    port: u16,
    commands: Arc<Mutex<Vec<String>>>,
    /// How often the receiver got an `AppCommand` request.
    app_commands: Arc<AtomicUsize>,
    /// How often the receiver got an `AppCommand0300` request.
    audyssey_reads: Arc<AtomicUsize>,
    pages: Arc<Pages>,
    /// The receiver is gone from its address: it closes every connection.
    gone: Arc<AtomicBool>,
    /// The tasks that accept connections. They end when the receiver leaves.
    accepts: Vec<AbortHandle>,
}

/// The description with empty serial numbers. Each part keeps its `serialNumber` element: the
/// library chooses the part by it.
fn description_with_empty_serial() -> Vec<u8> {
    String::from_utf8_lossy(DESCRIPTION)
        .replace(
            "<serialNumber>XXX</serialNumber>",
            "<serialNumber></serialNumber>",
        )
        .into_bytes()
}

/// The description with another manufacturer: the manufacturer is the only difference.
fn description_by(manufacturer: &str) -> Vec<u8> {
    String::from_utf8_lossy(DESCRIPTION)
        .replace(
            "<manufacturer>Denon</manufacturer>",
            &format!("<manufacturer>{manufacturer}</manufacturer>"),
        )
        .into_bytes()
}

impl Fake {
    /// Start a Denon receiver. The main zone is on if `power_on`.
    pub async fn start(power_on: bool) -> Self {
        Self::start_with(power_on, DESCRIPTION.to_vec(), Some(INFO)).await
    }

    /// Start a Marantz receiver. The main zone is on.
    pub async fn start_marantz() -> Self {
        Self::start_with(true, description_by("Marantz"), Some(INFO)).await
    }

    /// Start a receiver whose description names no manufacturer. The main zone is on.
    pub async fn start_without_brand() -> Self {
        Self::start_with(true, description_by(""), Some(INFO)).await
    }

    /// Start a receiver that does not give its description. The main zone is on.
    pub async fn start_without_description() -> Self {
        Self::start_with(true, Vec::new(), Some(INFO)).await
    }

    /// Start a receiver whose description has an empty serial number. The main zone is on.
    pub async fn start_with_empty_serial() -> Self {
        Self::start_with(true, description_with_empty_serial(), Some(INFO)).await
    }

    /// Start a newer receiver that gives no MAC address. The main zone is on.
    pub async fn start_without_mac() -> Self {
        Self::start_with(true, DESCRIPTION.to_vec(), Some(INFO_WITHOUT_MAC)).await
    }

    /// Start an old receiver: it has no device information, so no MAC address. The main zone is
    /// on.
    pub async fn start_old() -> Self {
        Self::start_with(true, DESCRIPTION.to_vec(), Some(OLD_INFO)).await
    }

    /// Start an old receiver whose description has an empty serial number. The main zone is on.
    pub async fn start_old_with_empty_serial() -> Self {
        Self::start_with(true, description_with_empty_serial(), Some(OLD_INFO)).await
    }

    /// Start a receiver with this device information. The main zone is on.
    pub async fn start_with_info(device_info: &'static [u8]) -> Self {
        Self::start_with(true, DESCRIPTION.to_vec(), Some(device_info)).await
    }

    /// Start a busy receiver: it answers the device information with HTTP 500. The main zone is
    /// on.
    pub async fn start_busy() -> Self {
        Self::start_with(true, DESCRIPTION.to_vec(), None).await
    }

    async fn start_with(
        power_on: bool,
        description: Vec<u8>,
        device_info: Option<&'static [u8]>,
    ) -> Self {
        let pages = Arc::new(Pages {
            description,
            device_info,
            description_reads: AtomicUsize::new(0),
            read_delay_ms: AtomicU64::new(0),
            command_delay_ms: AtomicU64::new(0),
            audyssey_answers: AtomicUsize::new(0),
            main_source: Mutex::new("MPLAY"),
        });
        let (port, listeners) = bind_loopback(0, "bind the fake receiver").await;
        let commands = Arc::new(Mutex::new(Vec::new()));
        let app_commands = Arc::new(AtomicUsize::new(0));
        let audyssey_reads = Arc::new(AtomicUsize::new(0));
        let gone = Arc::new(AtomicBool::new(false));
        let mut accepts = Vec::with_capacity(listeners.len());
        for listener in listeners {
            let (seen, asked, closed) = (commands.clone(), app_commands.clone(), gone.clone());
            let audyssey = audyssey_reads.clone();
            let pages = pages.clone();
            let task = tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    if closed.load(Ordering::SeqCst) {
                        continue;
                    }
                    tokio::spawn(serve(
                        socket,
                        power_on,
                        seen.clone(),
                        pages.clone(),
                        asked.clone(),
                        audyssey.clone(),
                        closed.clone(),
                    ));
                }
            });
            accepts.push(task.abort_handle());
        }
        Self {
            port,
            commands,
            app_commands,
            audyssey_reads,
            pages,
            gone,
            accepts,
        }
    }

    /// The receiver leaves its address: the port is closed and open connections get no answer.
    pub fn leave(&self) {
        self.gone.store(true, Ordering::SeqCst);
        for task in &self.accepts {
            task.abort();
        }
    }

    /// The port of the receiver.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// How often the receiver got an `AppCommand` request: the reads of the state.
    pub fn app_commands(&self) -> usize {
        self.app_commands.load(Ordering::SeqCst)
    }

    /// The receiver waits this long before it answers a read of the state, like a slow start.
    pub fn set_read_delay(&self, delay: Duration) {
        let ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.pages.read_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// The receiver waits this long before it answers a command. The command is recorded after the
    /// wait, also when the client is gone.
    pub fn set_command_delay(&self, delay: Duration) {
        let ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.pages.command_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// The receiver answers reads of the state with this main-zone source. It is `MPLAY` at the
    /// start.
    pub fn set_main_source(&self, source: &'static str) {
        *self.pages.main_source.lock().expect("lock") = source;
    }

    /// How often the receiver was asked for its description: once for each connect that looks the
    /// receiver up.
    pub fn description_reads(&self) -> usize {
        self.pages.description_reads.load(Ordering::SeqCst)
    }

    /// How often the receiver got an `AppCommand0300` request: the reads of Audyssey and the
    /// signal.
    pub fn audyssey_reads(&self) -> usize {
        self.audyssey_reads.load(Ordering::SeqCst)
    }

    /// How often an `AppCommand0300` request was answered. The answer comes 400 ms after the
    /// request.
    pub fn audyssey_answers(&self) -> usize {
        self.pages.audyssey_answers.load(Ordering::SeqCst)
    }

    /// The commands that the receiver got, as they are in the path: a space is `%20`. A command
    /// that waits for [`Fake::set_command_delay`] is not in the list yet.
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().expect("lock").clone()
    }

    /// An `Avr` that talks to this receiver over HTTP, with the state read once. Telnet is not
    /// connected, so the commands go over HTTP.
    pub async fn avr(&self) -> Avr {
        let avr = Avr::builder("127.0.0.1")
            .http_config(quick_http())
            .probes(&[(ReceiverType::AvrX, self.port)])
            .description_port(self.port)
            .follow_up_read(None)
            .connect()
            .await
            .expect("connect to the fake receiver");
        avr.update().await.expect("read the fake receiver");
        avr
    }
}

/// Listeners on the IPv4 loopback address and, if the system has it, on the IPv6 one, with the same
/// port. A connect to `localhost` then works at once on every system: Windows tries `::1` first and
/// waits about 2 seconds when nothing listens there.
async fn bind_loopback(port: u16, what: &str) -> (u16, Vec<TcpListener>) {
    let v4 = TcpListener::bind(("127.0.0.1", port)).await.expect(what);
    let port = v4.local_addr().expect("address").port();
    let mut listeners = vec![v4];
    if let Ok(v6) = TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    (port, listeners)
}

/// What differs between the fake receivers.
struct Pages {
    /// Empty: the description is not found.
    description: Vec<u8>,
    /// `None`: the device information is answered with HTTP 500.
    device_info: Option<&'static [u8]>,
    /// How often the description was asked for.
    description_reads: AtomicUsize,
    /// Wait before the answer to a read of the state, in milliseconds.
    read_delay_ms: AtomicU64,
    /// Wait before the answer to a command, in milliseconds.
    command_delay_ms: AtomicU64,
    /// How often an `AppCommand0300` request was answered.
    audyssey_answers: AtomicUsize,
    /// The main-zone source in the answer to a read of the state.
    main_source: Mutex<&'static str>,
}

async fn serve(
    mut socket: TcpStream,
    power_on: bool,
    commands: Arc<Mutex<Vec<String>>>,
    pages: Arc<Pages>,
    app_commands: Arc<AtomicUsize>,
    audyssey_reads: Arc<AtomicUsize>,
    gone: Arc<AtomicBool>,
) {
    let mut buffer = Vec::new();
    while let Some((method, path, body_length)) = read_head(&mut socket, &mut buffer).await {
        if gone.load(Ordering::SeqCst) {
            return;
        }
        let mut body = vec![0_u8; body_length];
        let have = buffer.len().min(body_length);
        body[..have].copy_from_slice(&buffer[..have]);
        buffer.drain(..have);
        if have < body_length && socket.read_exact(&mut body[have..]).await.is_err() {
            return;
        }
        if pages.device_info.is_none() && path == "/goform/Deviceinfo.xml" {
            let busy = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
            if socket.write_all(busy.as_bytes()).await.is_err() {
                return;
            }
            continue;
        }
        let answer: Option<Vec<u8>> = match (method.as_str(), path.as_str()) {
            ("GET", "/goform/Deviceinfo.xml") => pages.device_info.map(<[u8]>::to_vec),
            ("GET", "/description.xml") if !pages.description.is_empty() => {
                pages.description_reads.fetch_add(1, Ordering::SeqCst);
                Some(pages.description.clone())
            }
            ("GET", path) if path.starts_with(COMMAND) => {
                let delay = pages.command_delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                commands
                    .lock()
                    .expect("lock")
                    .push(path[COMMAND.len()..].to_owned());
                Some(Vec::new())
            }
            ("POST", "/goform/AppCommand.xml") => {
                app_commands.fetch_add(1, Ordering::SeqCst);
                let delay = pages.read_delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                let source = *pages.main_source.lock().expect("lock");
                Some(answer_for(&body, power_on, source))
            }
            // Every receiver that is read with `AppCommand` is asked. A receiver with Dirac Live
            // answers the Audyssey values blank.
            // It answers late, like a busy receiver: after the telnet answers of the start.
            ("POST", "/goform/AppCommand0300.xml") => {
                audyssey_reads.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(400)).await;
                pages.audyssey_answers.fetch_add(1, Ordering::SeqCst);
                Some(APPCOMMAND0300.to_vec())
            }
            _ => None,
        };
        let head = answer.as_ref().map_or_else(
            || "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_owned(),
            |body| {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
            },
        );
        if socket.write_all(head.as_bytes()).await.is_err()
            || socket
                .write_all(answer.as_deref().unwrap_or_default())
                .await
                .is_err()
        {
            return;
        }
    }
}

/// Read the head of a request. Returns the method, the path and the length of the body. The
/// start of the body stays in the buffer.
async fn read_head(
    socket: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Option<(String, String, usize)> {
    let end = loop {
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let mut chunk = [0_u8; 1024];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    buffer.drain(..end);
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    Some((method, path, length))
}

/// A fake web interface (port 11080 of a receiver). It answers every page with one status and
/// records the paths. `start_slow` answers after a delay, `start_slow_after` only for later
/// requests of one page.
pub struct FakeWeb {
    port: u16,
    paths: Arc<Mutex<Vec<String>>>,
}

impl FakeWeb {
    pub async fn start(status: u16) -> Self {
        Self::start_slow(status, Duration::ZERO).await
    }

    pub async fn start_slow(status: u16, delay: Duration) -> Self {
        Self::start_slow_after(status, delay, None).await
    }

    /// With `slow_after` = `Some((page, fast))`: answers the first `fast` requests of `page` at
    /// once, its later requests after `delay` and other pages at once. With `None`: answers all
    /// requests after `delay`.
    pub async fn start_slow_after(
        status: u16,
        delay: Duration,
        slow_after: Option<(&'static str, usize)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake web interface");
        let port = listener.local_addr().expect("address").port();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seen = paths.clone();
        let page_requests = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let seen = seen.clone();
                let page_requests = page_requests.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    while let Some((_, path, _)) = read_head(&mut socket, &mut buffer).await {
                        let slow = match slow_after {
                            None => true,
                            Some((page, fast)) => {
                                page == path && page_requests.fetch_add(1, Ordering::SeqCst) >= fast
                            }
                        };
                        seen.lock().expect("lock").push(path);
                        if !delay.is_zero() && slow {
                            tokio::time::sleep(delay).await;
                        }
                        let head = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
                        if socket.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self { port, paths }
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    /// How often a page was asked for.
    pub fn requests(&self, path: &str) -> usize {
        self.paths
            .lock()
            .expect("lock")
            .iter()
            .filter(|p| *p == path)
            .count()
    }
}

impl Fake {
    /// Like [`Fake::avr`], with a web interface on another port.
    pub async fn avr_with_web(&self, web: &FakeWeb) -> Avr {
        Avr::builder("127.0.0.1")
            .http_config(quick_http())
            .web_interface_config(quick_http())
            .web_interface_port(web.port)
            .probes(&[(ReceiverType::AvrX, self.port)])
            .description_port(self.port)
            .follow_up_read(None)
            .connect()
            .await
            .expect("connect to the fake receiver")
    }
}

/// Short timeouts and no pacing, so that the tests are quick.
pub fn quick_http() -> HttpConfig {
    HttpConfig {
        connect_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_millis(500),
        pacing: Pacing::Fixed {
            interval: Duration::ZERO,
        },
        ..HttpConfig::default()
    }
}

/// The lines of the fake telnet port for a line of the client.
type Answers = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// What the fake telnet port does on request of a test.
enum Control {
    Send(String),
    Close,
}

/// A fake telnet port of a receiver. It answers queries as the test says, sends lines and closes
/// the connection on request, and records what the client sends.
pub struct FakeTelnet {
    pub port: u16,
    received: Arc<Mutex<Vec<String>>>,
    control: Arc<Mutex<Option<mpsc::UnboundedSender<Control>>>>,
    connections: Arc<AtomicUsize>,
    /// New connections are closed at once.
    gone: Arc<AtomicBool>,
    /// The tasks that accept connections. They end when the receiver leaves.
    accepts: Vec<AbortHandle>,
}

impl FakeTelnet {
    /// Start the port. `answers` gives the lines for a line of the client.
    pub async fn start(answers: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self {
        Self::start_at(0, answers).await
    }

    /// As [`FakeTelnet::start`], on `port`. 0 picks a free port.
    pub async fn start_at(
        port: u16,
        answers: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        let (port, listeners) = bind_loopback(port, "bind the fake telnet port").await;
        let received = Arc::new(Mutex::new(Vec::new()));
        let control = Arc::new(Mutex::new(None));
        let connections = Arc::new(AtomicUsize::new(0));
        let gone = Arc::new(AtomicBool::new(false));
        let answers: Answers = Arc::new(answers);
        let mut accepts = Vec::with_capacity(listeners.len());
        for listener in listeners {
            let (seen, slot, count) = (received.clone(), control.clone(), connections.clone());
            let (closed, answers) = (gone.clone(), answers.clone());
            let task = tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    if closed.load(Ordering::SeqCst) {
                        continue;
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let (tx, rx) = mpsc::unbounded_channel();
                    *slot.lock().expect("lock") = Some(tx);
                    tokio::spawn(serve_telnet(socket, answers.clone(), seen.clone(), rx));
                }
            });
            accepts.push(task.abort_handle());
        }
        Self {
            port,
            received,
            control,
            connections,
            gone,
            accepts,
        }
    }

    /// Close the connection and the port.
    pub fn leave(&self) {
        self.gone.store(true, Ordering::SeqCst);
        for task in &self.accepts {
            task.abort();
        }
        self.close();
    }

    /// The lines that the client sent, in order.
    pub fn received(&self) -> Vec<String> {
        self.received.lock().expect("lock").clone()
    }

    /// How often the client sent this line.
    pub fn count(&self, line: &str) -> usize {
        self.received().iter().filter(|l| *l == line).count()
    }

    /// The number of connections that the client opened.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Send a line to the client, like an event of the receiver.
    pub fn send(&self, line: &str) {
        self.tell(Control::Send(format!("{line}\r")));
    }

    /// Close the connection.
    pub fn close(&self) {
        self.tell(Control::Close);
    }

    fn tell(&self, control: Control) {
        if let Some(tx) = self.control.lock().expect("lock").as_ref() {
            let _ = tx.send(control);
        }
    }
}

async fn serve_telnet(
    socket: TcpStream,
    answers: Answers,
    received: Arc<Mutex<Vec<String>>>,
    mut control: mpsc::UnboundedReceiver<Control>,
) {
    let (mut reader, mut writer) = socket.into_split();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        tokio::select! {
            read = reader.read(&mut chunk) => {
                let Ok(read) = read else { return };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                while let Some(end) = buffer.iter().position(|b| *b == b'\r') {
                    let line = String::from_utf8_lossy(&buffer[..end]).trim().to_owned();
                    buffer.drain(..=end);
                    if line.is_empty() {
                        continue;
                    }
                    for answer in answers(&line) {
                        if writer.write_all(format!("{answer}\r").as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    received.lock().expect("lock").push(line);
                }
            }
            order = control.recv() => match order {
                Some(Control::Send(text)) => {
                    if writer.write_all(text.as_bytes()).await.is_err() {
                        return;
                    }
                }
                Some(Control::Close) | None => return,
            },
        }
    }
}

/// The settings of the library for a test: the fake receiver and a fast telnet connection.
pub fn test_builder(
    http_port: u16,
    telnet_port: u16,
    builder: denon_marantz::AvrBuilder,
) -> denon_marantz::AvrBuilder {
    let telnet_config = TelnetConfig {
        port: telnet_port,
        connect_timeout: Duration::from_millis(500),
        reconnect_min: Duration::from_millis(100),
        reconnect_max: Duration::from_millis(300),
        command_interval: Duration::from_millis(5),
        query_interval: Duration::from_millis(5),
        query_quiet: Duration::from_millis(5),
        // The fakes answer at once. A query that a fake leaves out does not slow the test.
        answer_quiet: Duration::from_millis(300),
        ..TelnetConfig::default()
    };
    builder
        .http_config(quick_http())
        .web_interface_config(quick_http())
        .probes(&[(ReceiverType::AvrX, http_port)])
        .description_port(http_port)
        // The fake answers the pages of the web interface with 404.
        .web_interface_port(http_port)
        .telnet_config(telnet_config)
        .heos_config(
            crate::device::heos::TEST_HEOS
                .with(std::cell::Cell::get)
                .unwrap_or_default(),
        )
        .follow_up_read(None)
}

/// What the fake HEOS player plays.
#[derive(Debug, Clone)]
pub struct HeosItem {
    /// `play`, `pause` or `stop`.
    pub state: &'static str,
    /// `song` or `station`.
    pub kind: &'static str,
    pub song: &'static str,
    pub station: &'static str,
    pub artist: &'static str,
    pub album: &'static str,
    pub image_url: &'static str,
    /// HEOS source id: 3 is `TuneIn`, 1024 is local music.
    pub sid: u32,
}

impl HeosItem {
    /// A song that plays.
    pub const fn song() -> Self {
        Self {
            state: "play",
            kind: "song",
            song: "A Song",
            station: "",
            artist: "An Artist",
            album: "An Album",
            image_url: "http://127.0.0.1/cover.jpg",
            sid: 1024,
        }
    }

    /// A `TuneIn` station that plays. It cannot pause, skip or go back.
    pub const fn station() -> Self {
        Self {
            state: "play",
            kind: "station",
            song: "A Stream",
            station: "A Station",
            artist: "",
            album: "",
            image_url: "",
            sid: 3,
        }
    }
}

/// The player id of the fake HEOS player.
pub const HEOS_PID: i64 = 7;

/// A fake HEOS port of a receiver. Its one player has the address `ip`. It answers the reads with
/// `item`, answers commands with success and records the command lines.
pub struct FakeHeos {
    pub port: u16,
    item: Arc<Mutex<HeosItem>>,
    received: Arc<Mutex<Vec<String>>>,
    events: tokio::sync::broadcast::Sender<String>,
    /// Commands that change the play state get no answer.
    hang: Arc<AtomicBool>,
    /// The address of the player.
    ip: Arc<Mutex<&'static str>>,
}

impl FakeHeos {
    pub async fn start(ip: &'static str, item: HeosItem) -> Self {
        Self::start_at(0, ip, item).await
    }

    /// As [`FakeHeos::start`], on `port`. 0 picks a free port.
    pub async fn start_at(port: u16, ip: &'static str, item: HeosItem) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind the fake HEOS port");
        let port = listener.local_addr().expect("address").port();
        let item = Arc::new(Mutex::new(item));
        let received = Arc::new(Mutex::new(Vec::new()));
        let events = tokio::sync::broadcast::Sender::new(16);
        let hang = Arc::new(AtomicBool::new(false));
        let ip = Arc::new(Mutex::new(ip));
        let (state, seen, sender) = (item.clone(), received.clone(), events.clone());
        let (silent, address) = (hang.clone(), ip.clone());
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve_heos(
                    socket,
                    address.clone(),
                    state.clone(),
                    seen.clone(),
                    sender.clone(),
                    silent.clone(),
                ));
            }
        });
        Self {
            port,
            item,
            received,
            events,
            hang,
            ip,
        }
    }

    /// Give the player another address, and tell the client that the players changed.
    pub fn move_player(&self, ip: &'static str) {
        *self.ip.lock().expect("lock") = ip;
        let _ = self
            .events
            .send(r#"{"heos": {"command": "event/players_changed", "message": ""}}"#.to_owned());
    }

    /// From now on, commands that change the play state get no answer.
    pub fn hang(&self) {
        self.hang.store(true, Ordering::SeqCst);
    }

    /// How often the client sent this command, for example `player/get_players`.
    pub fn count(&self, command: &str) -> usize {
        self.received()
            .iter()
            .filter(|l| l.split('?').next() == Some(command))
            .count()
    }

    /// Settings of the HEOS client for this port, with short times.
    pub const fn config(&self) -> denon_marantz::heos::HeosConfig {
        denon_marantz::heos::HeosConfig {
            port: self.port,
            connect_timeout: Duration::from_millis(500),
            write_timeout: Duration::from_millis(500),
            command_timeout: Duration::from_millis(500),
            slow_command_limit: Duration::from_millis(1000),
            keep_alive: Duration::from_secs(10),
            reconnect_min: Duration::from_millis(50),
            reconnect_max: Duration::from_millis(200),
            now_playing_delay: Duration::from_millis(10),
        }
    }

    /// The command lines that the client sent, without `heos://`.
    pub fn received(&self) -> Vec<String> {
        self.received.lock().expect("lock").clone()
    }

    /// Change what plays and send the events of the change.
    pub fn play(&self, item: HeosItem) {
        let state = item.state;
        *self.item.lock().expect("lock") = item;
        for line in [
            format!(
                r#"{{"heos": {{"command": "event/player_state_changed", "message": "pid={HEOS_PID}&state={state}"}}}}"#
            ),
            format!(
                r#"{{"heos": {{"command": "event/player_now_playing_changed", "message": "pid={HEOS_PID}"}}}}"#
            ),
        ] {
            let _ = self.events.send(line);
        }
    }
}

async fn serve_heos(
    socket: TcpStream,
    ip: Arc<Mutex<&'static str>>,
    item: Arc<Mutex<HeosItem>>,
    received: Arc<Mutex<Vec<String>>>,
    events: tokio::sync::broadcast::Sender<String>,
    hang: Arc<AtomicBool>,
) {
    use tokio::io::AsyncBufReadExt;
    let (reader, mut writer) = socket.into_split();
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let mut registered: Option<tokio::sync::broadcast::Receiver<String>> = None;
    loop {
        let event = async {
            match registered.as_mut() {
                Some(events) => events.recv().await.ok(),
                None => std::future::pending().await,
            }
        };
        let out = tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { return };
                let line = line.trim_end().trim_start_matches("heos://").to_owned();
                let (name, args) = line.split_once('?').unwrap_or((&line, ""));
                let answer = heos_answer(name, args, *ip.lock().expect("lock"), &item.lock().expect("lock"));
                if name == "system/register_for_change_events" && args.contains("enable=on") {
                    registered = Some(events.subscribe());
                }
                received.lock().expect("lock").push(line.clone());
                if name == "player/set_play_state" && hang.load(Ordering::SeqCst) {
                    continue;
                }
                answer
            }
            Some(line) = event => line,
        };
        if writer
            .write_all(format!("{out}\r\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
    }
}

fn heos_answer(name: &str, args: &str, ip: &str, item: &HeosItem) -> String {
    let head = |message: &str| {
        format!(r#""heos": {{"command": "{name}", "result": "success", "message": "{message}"}}"#)
    };
    match name {
        "player/get_players" => format!(
            r#"{{{}, "payload": [{{"name": "AVR", "pid": {HEOS_PID}, "model": "Denon AVR", "ip": "{ip}", "network": "wired"}}]}}"#,
            head("")
        ),
        "player/get_play_state" => format!(
            "{{{}}}",
            head(&format!("pid={HEOS_PID}&state={}", item.state))
        ),
        "player/get_play_mode" => format!(
            "{{{}}}",
            head(&format!("pid={HEOS_PID}&repeat=off&shuffle=off"))
        ),
        "player/get_now_playing_media" => format!(
            r#"{{{}, "payload": {{"type": "{}", "song": "{}", "station": "{}", "album": "{}", "artist": "{}", "image_url": "{}", "qid": 1, "sid": {}}}, "options": []}}"#,
            head(&format!("pid={HEOS_PID}")),
            item.kind,
            item.song,
            item.station,
            item.album,
            item.artist,
            item.image_url,
            item.sid
        ),
        _ => format!("{{{}}}", head(args)),
    }
}
