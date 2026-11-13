// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Test program that acts as the Remote. It starts the driver, sets it up with a real receiver
//! and runs the checks of the guided manual test over the WebSocket. The volume is never set
//! above the limit in `checks::MAX_VOLUME`.
//!
//! Usage: `remote-sim <receiver address> [--driver <path>] [--only a,b] [--skip a,b]
//! [--kinds a,b] [--no-power] [--no-oracle] [--port <port>] [--commands <path>]`

mod checks;
mod driver;
mod oracle;
mod ws;

use driver::Driver;
use std::fmt::{self, Display};
use std::path::PathBuf;
use std::time::Instant;
use ws::Client;

/// A failed or skipped check.
pub struct Fail {
    pub msg: String,
    pub skip: bool,
}

impl Fail {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            skip: false,
        }
    }

    pub fn skip(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            skip: true,
        }
    }
}

impl<E: std::error::Error> From<E> for Fail {
    fn from(e: E) -> Self {
        Self::new(e.to_string())
    }
}

impl Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

/// Result of a check: a note for the report.
pub type Check = Result<String, Fail>;

pub struct Options {
    pub address: String,
    pub bin: PathBuf,
    pub only: Vec<String>,
    pub skip: Vec<String>,
    pub no_power: bool,
    /// No second connection to the receiver: the values are not compared with it.
    pub no_oracle: bool,
    pub port: u16,
    pub commands: PathBuf,
    /// Selects for the sweep. Empty: all.
    pub kinds: Vec<String>,
}

pub struct Ctx {
    pub opt: Options,
    pub client: Client,
    pub driver: Driver,
    /// A second connection to the receiver, for the values that it has.
    pub oracle: Option<oracle::Oracle>,
    /// Device key: the serial number of the receiver.
    pub key: String,
    /// The receiver was on at the start.
    pub was_on: bool,
    report: Vec<String>,
    failed: usize,
    skipped: usize,
}

impl Ctx {
    pub fn mp(&self) -> String {
        format!("media_player.{}", self.key)
    }

    pub fn remote(&self) -> String {
        format!("remote.{}", self.key)
    }

    pub fn select(&self, kind: &str) -> String {
        format!("select.{kind}.{}", self.key)
    }

    pub fn sensor(&self, kind: &str) -> String {
        format!("sensor.{kind}.{}", self.key)
    }

    /// Connect a new Remote, like a Remote that wakes up.
    pub async fn reconnect(&mut self) -> Result<(), Fail> {
        self.client = Client::connect(self.driver.port).await?;
        self.client.send_event("connect").await
    }

    fn selected(&self, name: &str) -> bool {
        (self.opt.only.is_empty() || self.opt.only.iter().any(|n| n == name))
            && !self.opt.skip.iter().any(|n| n == name)
    }

    /// Run a check and print the result. Returns false for a failure.
    async fn step(&mut self, name: &str, check: impl AsyncFnOnce(&mut Self) -> Check) -> bool {
        if !self.selected(name) {
            return true;
        }
        let start = Instant::now();
        let result = check(self).await;
        let ms = start.elapsed().as_millis();
        let (tag, note) = match result {
            Ok(note) => ("PASS", note),
            Err(f) if f.skip => {
                self.skipped += 1;
                ("SKIP", f.msg)
            }
            Err(f) => {
                self.failed += 1;
                ("FAIL", f.msg)
            }
        };
        let line = if note.is_empty() {
            format!("{tag}  {name} ({ms} ms)")
        } else {
            format!("{tag}  {name} ({ms} ms): {note}")
        };
        println!("{line}");
        self.report.push(line);
        tag != "FAIL"
    }
}

fn parse() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut opt = Options {
        address: String::new(),
        bin: PathBuf::from("target/release/uc-intg-denonmarantz"),
        only: Vec::new(),
        skip: Vec::new(),
        no_power: false,
        no_oracle: false,
        port: 19090,
        commands: PathBuf::from("resources/commands.tsv"),
        kinds: Vec::new(),
    };
    let list = |v: Option<String>| -> Vec<String> {
        v.unwrap_or_default()
            .split(',')
            .map(str::to_owned)
            .collect()
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--driver" => opt.bin = args.next().ok_or("--driver needs a path")?.into(),
            "--only" => opt.only = list(args.next()),
            "--skip" => opt.skip = list(args.next()),
            "--commands" => opt.commands = args.next().ok_or("--commands needs a path")?.into(),
            "--kinds" => opt.kinds = list(args.next()),
            "--no-oracle" => opt.no_oracle = true,
            "--no-power" => opt.no_power = true,
            "--port" => {
                opt.port = args
                    .next()
                    .and_then(|p| p.parse().ok())
                    .ok_or("--port needs a number")?;
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => opt.address = other.to_owned(),
        }
    }
    if opt.address.is_empty() {
        return Err("the receiver address is missing".into());
    }
    Ok(opt)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opt = match parse() {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "{e}\nUsage: remote-sim <receiver address> [--driver <path>] [--only a,b] [--skip a,b] [--kinds a,b] [--no-power] [--no-oracle] [--port <port>] [--commands <path>]"
            );
            std::process::exit(2);
        }
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dir = std::env::temp_dir().join(format!("remote-sim-{stamp}"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        std::process::exit(2);
    }
    let mut driver = Driver::new(&opt.bin, &dir, opt.port);
    if let Err(e) = driver.start().await {
        eprintln!("{e}");
        std::process::exit(2);
    }
    let client = match Client::connect(opt.port).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    println!("Driver log and configuration: {}", dir.display());
    let mut ctx = Ctx {
        opt,
        client,
        driver,
        oracle: None,
        key: String::new(),
        was_on: true,
        report: Vec::new(),
        failed: 0,
        skipped: 0,
    };
    run(&mut ctx).await;
    if let Some(oracle) = &ctx.oracle {
        oracle.close().await;
    }
    ctx.driver.stop().await;
    let summary = format!("{} checks failed, {} skipped", ctx.failed, ctx.skipped);
    println!("\n{summary}");
    ctx.report.push(String::new());
    ctx.report.push(summary);
    let file = format!("remote-sim-results-{stamp}.md");
    if std::fs::write(&file, ctx.report.join("\n") + "\n").is_ok() {
        println!("Results: {file}");
    }
    std::process::exit(i32::from(ctx.failed > 0));
}

async fn run(ctx: &mut Ctx) {
    // A failed setup or connect makes all other checks fail, so the run ends there.
    if !ctx.step("metadata", checks::metadata).await
        || !ctx.step("setup", checks::setup_first).await
        || !ctx.step("entities", checks::entities).await
        || !ctx.step("states", checks::states).await
    {
        return;
    }
    ctx.step("values", checks::values).await;
    ctx.step("remote_ui", checks::remote_ui).await;
    ctx.step("standby", checks::standby).await;
    ctx.step("remote_wake", checks::remote_wake).await;
    ctx.step("selects", checks::selects).await;
    ctx.step("remote_commands", checks::remote_commands).await;
    ctx.step("mute", checks::mute).await;
    ctx.step("source", checks::source).await;
    ctx.step("sound_mode", checks::sound_mode).await;
    ctx.step("volume", checks::volume).await;
    ctx.step("power", checks::power).await;
    ctx.step("http_mode", checks::http_mode).await;
    ctx.step("backup_restore", checks::backup_restore).await;
    ctx.step("unreachable", checks::unreachable).await;
    ctx.step("config_file", checks::config_file).await;
    ctx.step("python_config", checks::python_config).await;
    ctx.step("broken_config", checks::broken_config).await;
    ctx.step("select_sweep", checks::select_sweep).await;
    ctx.step("extra_entities", checks::extra_entities).await;
    ctx.step("simple_commands", checks::simple_commands).await;
    ctx.step("restore_power", checks::restore_power).await;
    ctx.step("log", checks::log).await;
}
