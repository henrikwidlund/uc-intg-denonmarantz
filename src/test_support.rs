// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! A fake receiver for the tests: a small HTTP server. It answers like a receiver of the AVR-X
//! family and records the commands that it gets.

use denon_marantz::{Avr, HttpConfig, Pacing, ReceiverType, TelnetConfig};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const DESCRIPTION: &[u8] = include_bytes!("test_data/description.xml");
const INFO: &[u8] =
    b"<Device_Info><CommApiVers>0301</CommApiVers><ModelName>AVC-X3700H</ModelName>\
    <DeviceZones>2</DeviceZones><MacAddress>0123456789AB</MacAddress></Device_Info>";
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
    // Only receivers of the 2016 type are asked.
    "GetToneControl",
];

/// The answer to an `AppCommand` request: the items for the queries that the request has, in the
/// order of the request.
fn answer_for(request: &[u8], power_on: bool) -> Vec<u8> {
    let whole = String::from_utf8(update_answer(power_on)).expect("text");
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

/// The answer to the update request: the main zone is on.
fn update_answer(power_on: bool) -> Vec<u8> {
    let power = if power_on { "ON" } else { "OFF" };
    format!(
        "<rx>\
        <cmd><zone1>{power}</zone1><zone2>OFF</zone2></cmd>\
        <cmd><list><listvalue><zone>Main</zone><value>60</value></listvalue>\
            <listvalue><zone>Zone2</zone><value>0</value></listvalue></list></cmd>\
        <cmd><value>2</value></cmd>\
        <cmd><mode>1</mode></cmd>\
        <cmd><zone1><source>MPLAY</source></zone1><zone2><source>CD</source></zone2></cmd>\
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
        <cmd><status>0</status><adjust>1</adjust><basslevel>0dB</basslevel>\
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
    /// The receiver is gone from its address: it closes every connection.
    gone: Arc<AtomicBool>,
}

/// The description of a Marantz receiver: the manufacturer is the only difference.
fn marantz_description() -> Vec<u8> {
    String::from_utf8_lossy(DESCRIPTION)
        .replace(
            "<manufacturer>Denon</manufacturer>",
            "<manufacturer>Marantz</manufacturer>",
        )
        .into_bytes()
}

impl Fake {
    /// Start a Denon receiver. The main zone is on if `power_on`.
    pub async fn start(power_on: bool) -> Self {
        Self::start_with(power_on, DESCRIPTION.to_vec()).await
    }

    /// Start a Marantz receiver. The main zone is on.
    pub async fn start_marantz() -> Self {
        Self::start_with(true, marantz_description()).await
    }

    async fn start_with(power_on: bool, description: Vec<u8>) -> Self {
        let description: Arc<[u8]> = description.into();
        let (port, listeners) = bind_loopback("bind the fake receiver").await;
        let commands = Arc::new(Mutex::new(Vec::new()));
        let app_commands = Arc::new(AtomicUsize::new(0));
        let gone = Arc::new(AtomicBool::new(false));
        for listener in listeners {
            let (seen, asked, closed) = (commands.clone(), app_commands.clone(), gone.clone());
            let description = description.clone();
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    if closed.load(Ordering::SeqCst) {
                        continue;
                    }
                    tokio::spawn(serve(
                        socket,
                        power_on,
                        seen.clone(),
                        description.clone(),
                        asked.clone(),
                        closed.clone(),
                    ));
                }
            });
        }
        Self {
            port,
            commands,
            app_commands,
            gone,
        }
    }

    /// The receiver leaves its address: it answers no request from now on.
    pub fn leave(&self) {
        self.gone.store(true, Ordering::SeqCst);
    }

    /// The port of the receiver.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// How often the receiver got an `AppCommand` request: the reads of the state.
    pub fn app_commands(&self) -> usize {
        self.app_commands.load(Ordering::SeqCst)
    }

    /// The commands that the receiver got, as they are in the path: a space is `%20`.
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
async fn bind_loopback(what: &str) -> (u16, Vec<TcpListener>) {
    let v4 = TcpListener::bind("127.0.0.1:0").await.expect(what);
    let port = v4.local_addr().expect("address").port();
    let mut listeners = vec![v4];
    if let Ok(v6) = TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    (port, listeners)
}

async fn serve(
    mut socket: TcpStream,
    power_on: bool,
    commands: Arc<Mutex<Vec<String>>>,
    description: Arc<[u8]>,
    app_commands: Arc<AtomicUsize>,
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
        let answer: Option<Vec<u8>> = match (method.as_str(), path.as_str()) {
            ("GET", "/goform/Deviceinfo.xml") => Some(INFO.to_vec()),
            ("GET", "/description.xml") => Some(description.to_vec()),
            ("GET", path) if path.starts_with(COMMAND) => {
                commands
                    .lock()
                    .expect("lock")
                    .push(path[COMMAND.len()..].to_owned());
                Some(Vec::new())
            }
            ("POST", "/goform/AppCommand.xml") => {
                app_commands.fetch_add(1, Ordering::SeqCst);
                Some(answer_for(&body, power_on))
            }
            // Only a receiver of the 2016 type is asked. A receiver with Dirac Live answers the
            // Audyssey values blank.
            // It answers late, like a busy receiver: after the telnet answers of the start.
            ("POST", "/goform/AppCommand0300.xml") => {
                tokio::time::sleep(Duration::from_millis(400)).await;
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
/// records the paths.
pub struct FakeWeb {
    port: u16,
    paths: Arc<Mutex<Vec<String>>>,
}

impl FakeWeb {
    pub async fn start(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake web interface");
        let port = listener.local_addr().expect("address").port();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seen = paths.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    while let Some((_, path, _)) = read_head(&mut socket, &mut buffer).await {
                        seen.lock().expect("lock").push(path);
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
fn quick_http() -> HttpConfig {
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
}

impl FakeTelnet {
    /// Start the port. `answers` gives the lines for a line of the client.
    pub async fn start(answers: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self {
        let (port, listeners) = bind_loopback("bind the fake telnet port").await;
        let received = Arc::new(Mutex::new(Vec::new()));
        let control = Arc::new(Mutex::new(None));
        let connections = Arc::new(AtomicUsize::new(0));
        let gone = Arc::new(AtomicBool::new(false));
        let answers: Answers = Arc::new(answers);
        for listener in listeners {
            let (seen, slot, count) = (received.clone(), control.clone(), connections.clone());
            let (closed, answers) = (gone.clone(), answers.clone());
            tokio::spawn(async move {
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
        }
        Self {
            port,
            received,
            control,
            connections,
            gone,
        }
    }

    /// Close the connection and refuse new ones.
    pub fn leave(&self) {
        self.gone.store(true, Ordering::SeqCst);
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
        .follow_up_read(None)
}
