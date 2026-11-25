// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Test program that acts as the Remote. It starts the driver, sets it up with a real receiver
//! and runs the checks of the guided manual test over the WebSocket. The volume is never set
//! above the limit in `checks::MAX_VOLUME`.
//!
//! Usage: `remote-sim <receiver address> [--driver <path>] [--only a,b] [--skip a,b]
//! [--kinds a,b] [--avoid-sources a,b] [--no-power] [--no-oracle] [--port <port>] [--commands <path>]`

mod checks;
mod driver;
mod oracle;
mod restore;
mod ws;

use driver::Driver;
use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use std::time::Instant;
use ws::Client;

/// All steps in the order of [`run`].
const STEPS: [&str; 29] = [
    "metadata",
    "setup",
    "entities",
    "states",
    "values",
    "heos",
    "remote_ui",
    "standby",
    "remote_wake",
    "unused_receiver",
    "selects",
    "remote_commands",
    "mute",
    "source",
    "sound_mode",
    "volume",
    "power",
    "http_mode",
    "backup_restore",
    "unreachable",
    "config_file",
    "python_config",
    "broken_config",
    "select_sweep",
    "extra_entities",
    "settings_restore",
    "simple_commands",
    "restore_power",
    "log",
];
/// Steps that set up the driver and connect it. The other checks need them.
const BASE_STEPS: [&str; 3] = ["setup", "entities", "states"];
/// Steps at the end that set the power and the mute state back to the start, lower the volume to
/// the start volume (not above -40 dB, only with the second connection) if it is higher, and check
/// the driver log. They always run.
const END_STEPS: [&str; 2] = ["restore_power", "log"];
/// Steps that run only if `--only` names them.
const OPT_IN_STEPS: [&str; 0] = [];

/// A step in [`END_STEPS`] always runs. Else `--skip` wins. Without `--only`, all steps except
/// [`OPT_IN_STEPS`] run. With `--only`, the named steps and [`BASE_STEPS`] run.
fn selected(only: &[String], skip: &[String], name: &str) -> bool {
    if END_STEPS.contains(&name) {
        return true;
    }
    if skip.iter().any(|n| n == name) {
        return false;
    }
    if only.is_empty() {
        !OPT_IN_STEPS.contains(&name)
    } else {
        BASE_STEPS.contains(&name) || only.iter().any(|n| n == name)
    }
}

/// The repository folder. Results and the restore file go there, whatever the current folder is.
pub fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tools/remote-sim is in the repository")
}

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
    /// Sources that the checks never select: a source whose name contains one of these, without
    /// case. Empty: the checks select no source at all.
    pub avoid_sources: Vec<String>,
}

impl Options {
    /// A source that the checks may select. Without sources to avoid, none: a source must never be
    /// selected by mistake.
    pub fn may_select(&self, source: &str) -> bool {
        if self.avoid_sources.is_empty() {
            return false;
        }
        let source = source.to_lowercase();
        !self
            .avoid_sources
            .iter()
            .any(|a| source.contains(&a.to_lowercase()))
    }
}

/// Mute and volume of the receiver at the start of the run.
pub struct Start {
    pub muted: Option<bool>,
    pub volume: Option<f64>,
}

pub struct Ctx {
    pub opt: Options,
    pub client: Client,
    pub driver: Driver,
    /// A second connection to the receiver, for the values that it has.
    pub oracle: Option<oracle::Oracle>,
    /// Device key: the MAC address or serial number of the receiver.
    pub key: String,
    /// The receiver was on at the start.
    pub was_on: bool,
    /// Mute and volume at the start of the run, as read by the `states` step. `None` before that
    /// step.
    pub start: Option<Start>,
    /// The driver shows the tuner entities: the setup option, else what `Deviceinfo.xml` says.
    pub tuner: bool,
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

    /// Connect a new Remote, like a Remote that wakes up. Like the core, it subscribes the
    /// entities before `connect`: the driver connects only receivers with subscribed entities.
    pub async fn reconnect(&mut self) -> Result<(), Fail> {
        self.client = Client::connect(self.driver.port).await?;
        let data = self
            .client
            .request_ok("get_available_entities", serde_json::Value::Null)
            .await?;
        let ids: Vec<&str> = data["available_entities"]
            .as_array()
            .map_or_default(|a| a.iter().filter_map(|e| e["entity_id"].as_str()).collect());
        self.client
            .request_ok("subscribe_events", serde_json::json!({ "entity_ids": ids }))
            .await?;
        self.client.send_event("connect").await
    }

    fn selected(&self, name: &str) -> bool {
        selected(&self.opt.only, &self.opt.skip, name)
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
    parse_from(std::env::args().skip(1))
}

/// Parse the arguments without the program name.
fn parse_from(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut opt = Options {
        address: String::new(),
        bin: repo_root().join("target/release/uc-intg-denonmarantz"),
        only: Vec::new(),
        skip: Vec::new(),
        no_power: false,
        no_oracle: false,
        port: 19090,
        commands: repo_root().join("resources/commands.tsv"),
        kinds: Vec::new(),
        avoid_sources: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--driver" => opt.bin = args.next().ok_or("--driver needs a path")?.into(),
            "--only" => opt.only.extend(step_list("--only", args.next())?),
            "--skip" => {
                let list = step_list("--skip", args.next())?;
                if let Some(base) = list.iter().find(|n| BASE_STEPS.contains(&n.as_str())) {
                    return Err(format!(
                        "--skip: {base} cannot be skipped, the other checks need it. Steps that cannot be skipped: {}",
                        BASE_STEPS.join(", ")
                    ));
                }
                opt.skip.extend(list);
            }
            "--commands" => opt.commands = args.next().ok_or("--commands needs a path")?.into(),
            "--kinds" => opt.kinds.extend(name_list("--kinds", args.next())?),
            "--avoid-sources" => {
                opt.avoid_sources
                    .extend(name_list("--avoid-sources", args.next())?);
            }
            "--no-oracle" => opt.no_oracle = true,
            "--no-power" => opt.no_power = true,
            "--port" => {
                opt.port = args
                    .next()
                    .and_then(|p| p.parse().ok())
                    .ok_or("--port needs a number")?;
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other if !opt.address.is_empty() => {
                return Err(format!(
                    "more than one receiver address: {} and {other}",
                    opt.address
                ));
            }
            other => opt.address = other.to_owned(),
        }
    }
    if opt.address.is_empty() {
        return Err("the receiver address is missing".into());
    }
    Ok(opt)
}

/// A note if `--skip` names an end step: end steps are not skipped.
fn skip_note(skip: &[String]) -> Option<String> {
    skip.iter()
        .any(|n| END_STEPS.contains(&n.as_str()))
        .then(|| format!("{} always run", END_STEPS.join(" and ")))
}

/// Names of `--kinds` or `--avoid-sources`, separated by commas. Spaces around a name are removed
/// and empty names are left out. Refuses a missing value, a value that starts with `-` and a value
/// without names.
fn name_list(flag: &str, value: Option<String>) -> Result<Vec<String>, String> {
    let value = value.ok_or_else(|| format!("{flag} needs names"))?;
    if value.starts_with('-') {
        return Err(format!("{flag} needs names, not the option {value}"));
    }
    let names: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    if names.is_empty() {
        return Err(format!("{flag}: no names in {value:?}"));
    }
    Ok(names)
}

/// Step names of `--only` or `--skip`, separated by commas. Refuses a missing value and unknown
/// or empty names.
fn step_list(flag: &str, value: Option<String>) -> Result<Vec<String>, String> {
    let value = value.ok_or_else(|| format!("{flag} needs step names"))?;
    value
        .split(',')
        .map(|name| {
            if STEPS.contains(&name) {
                Ok(name.to_owned())
            } else {
                Err(format!(
                    "{flag}: unknown step {name:?}. Steps: {}",
                    STEPS.join(", ")
                ))
            }
        })
        .collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opt = match parse() {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "{e}\nUsage: remote-sim <receiver address> [--driver <path>] [--only a,b] [--skip a,b] [--kinds a,b] [--avoid-sources a,b] [--no-power] [--no-oracle] [--port <port>] [--commands <path>]"
            );
            std::process::exit(2);
        }
    };
    if let Some(note) = skip_note(&opt.skip) {
        println!("{note}");
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dir = std::env::temp_dir().join(format!("remote-sim-{stamp}"));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        eprintln!("cannot create {}: {e}", dir.display());
        std::process::exit(2);
    }
    if let Some(note) = restore::after_early_stop(&opt.address).await {
        println!("{note}");
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
        start: None,
        tuner: true,
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
    let dir = repo_root().join("test-results");
    let file = dir.join(format!("remote-sim-results-{stamp}.md"));
    let written = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(&file, ctx.report.join("\n") + "\n").await
    };
    match written.await {
        Ok(()) => println!("Results: {}", file.display()),
        Err(e) => eprintln!("cannot write {}: {e}", file.display()),
    }
    std::process::exit(i32::from(ctx.failed > 0));
}

async fn run(ctx: &mut Ctx) {
    // A failed setup or connect makes the other checks fail, so only the end steps run then.
    if ctx.step("metadata", checks::metadata).await
        && ctx.step("setup", checks::setup_first).await
        && ctx.step("entities", checks::entities).await
        && ctx.step("states", checks::states).await
    {
        middle_steps(ctx).await;
    }
    ctx.step("restore_power", checks::restore_power).await;
    ctx.step("log", checks::log).await;
}

/// The checks between the base steps and the end steps.
async fn middle_steps(ctx: &mut Ctx) {
    ctx.step("values", checks::values).await;
    ctx.step("heos", checks::heos).await;
    ctx.step("remote_ui", checks::remote_ui).await;
    ctx.step("standby", checks::standby).await;
    ctx.step("remote_wake", checks::remote_wake).await;
    ctx.step("unused_receiver", checks::unused_receiver).await;
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
    ctx.step("settings_restore", checks::settings_restore).await;
    ctx.step("simple_commands", checks::simple_commands).await;
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Options for tests, with these sources to avoid.
    pub fn options(avoid: &[&str]) -> Options {
        Options {
            address: String::new(),
            bin: PathBuf::new(),
            only: Vec::new(),
            skip: Vec::new(),
            no_power: false,
            no_oracle: false,
            port: 0,
            commands: PathBuf::new(),
            kinds: Vec::new(),
            avoid_sources: avoid.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|&n| n.to_owned()).collect()
    }

    #[test]
    fn step_selection() {
        for name in STEPS {
            assert!(selected(&[], &[], name), "no --only runs {name}");
        }
        let only = names(&["volume"]);
        for name in [
            "volume",
            "setup",
            "entities",
            "states",
            "restore_power",
            "log",
        ] {
            assert!(selected(&only, &[], name), "{name}");
        }
        assert!(!selected(&only, &[], "metadata"));
        assert!(!selected(&only, &[], "mute"));
        assert!(!selected(&only, &only, "volume"), "--skip wins");
        assert!(!selected(&[], &names(&["mute"]), "mute"));
        let end = names(&["restore_power", "log"]);
        for name in END_STEPS {
            assert!(selected(&[], &end, name), "--skip keeps {name}");
            assert!(
                selected(&names(&["mute"]), &end, name),
                "--only keeps {name}"
            );
        }
    }

    #[test]
    fn step_lists_are_parts_of_steps() {
        for name in BASE_STEPS.iter().chain(&END_STEPS).chain(&OPT_IN_STEPS) {
            assert!(STEPS.contains(name), "{name}");
        }
        for name in OPT_IN_STEPS {
            assert!(
                !BASE_STEPS.contains(&name) && !END_STEPS.contains(&name),
                "{name}"
            );
            assert!(!selected(&[], &[], name), "{name} runs without --only");
        }
    }

    /// Body of the fn that starts with `head`.
    fn body<'a>(src: &'a str, head: &str) -> &'a str {
        let start = src.find(head).unwrap_or_else(|| panic!("{head}"));
        let len = src[start..].find("\n}\n").expect("end of fn");
        &src[start..start + len]
    }

    /// `STEPS` has the steps of `run` and `middle_steps` in the order they run.
    #[test]
    fn steps_match_run() {
        let src = include_str!("main.rs");
        let middle = body(src, "async fn middle_steps(");
        let raw_run = body(src, "async fn run(");
        // The end steps also run when a base step fails, so they come after the `if`.
        let call_at = raw_run
            .find("middle_steps(ctx).await;\n    }\n")
            .expect("end of if");
        for name in END_STEPS {
            let call = format!("ctx.step(\"{name}\"");
            assert!(!middle.contains(&call), "{name} in middle_steps");
            assert!(
                raw_run.find(&call).expect(name) > call_at,
                "{name} not after the if"
            );
        }
        let run = raw_run.replace("middle_steps(ctx).await", middle);
        let mut last = 0;
        for name in STEPS {
            let at = run
                .find(&format!("ctx.step(\"{name}\""))
                .unwrap_or_else(|| panic!("{name} not in run"));
            assert!(at > last, "{name} out of order");
            last = at;
        }
        assert_eq!(
            STEPS.len(),
            run.matches("ctx.step(\"").count(),
            "a step of run is not in STEPS"
        );
        // The base steps run before the middle steps; the end steps run last.
        let middle_at = run.find("async fn middle_steps(").expect("middle_steps");
        for name in BASE_STEPS {
            let at = run.find(&format!("ctx.step(\"{name}\"")).expect(name);
            assert!(at < middle_at, "{name} is not a base step");
        }
        assert!(STEPS.ends_with(&END_STEPS), "end steps are not last");
    }

    fn parse(args: &[&str]) -> Result<Options, String> {
        parse_from(args.iter().map(|&a| a.to_owned()))
    }

    #[test]
    fn step_names() {
        let only = |v: &str| step_list("--only", Some(v.to_owned()));
        assert_eq!(Ok(names(&["volume", "log"])), only("volume,log"));
        assert!(step_list("--only", None).is_err(), "missing value");
        for bad in ["", "volum", "volume,", "a,,b", ",volume"] {
            let e = only(bad).expect_err(bad);
            assert!(e.contains("Steps: metadata, setup"), "{e}");
        }
    }

    #[test]
    fn only_and_skip_can_repeat() {
        let opt = parse(&[
            "192.0.2.1",
            "--only",
            "mute",
            "--only",
            "volume,power",
            "--skip",
            "heos",
            "--skip",
            "log",
        ])
        .expect("valid");
        assert_eq!("192.0.2.1", opt.address);
        assert_eq!(names(&["mute", "volume", "power"]), opt.only);
        assert_eq!(names(&["heos", "log"]), opt.skip);
        assert!(parse(&["192.0.2.1", "--only", "mutee"]).is_err());
        assert!(parse(&["192.0.2.1", "--skip", ""]).is_err());
        assert!(parse(&["192.0.2.1", "--skip"]).is_err());
        let opt = parse(&[
            "192.0.2.1",
            "--avoid-sources",
            "oppo",
            "--avoid-sources",
            "bluray",
        ])
        .expect("valid");
        assert_eq!(names(&["oppo", "bluray"]), opt.avoid_sources);
        let opt = parse(&["192.0.2.1", "--kinds", "dimmer", "--kinds", "eco"]).expect("valid");
        assert_eq!(names(&["dimmer", "eco"]), opt.kinds);
    }

    #[test]
    fn list_names_are_trimmed_and_one_address_is_allowed() {
        let opt = parse(&["192.0.2.1", "--avoid-sources", "bluray, oppo"]).expect("valid");
        assert_eq!(names(&["bluray", "oppo"]), opt.avoid_sources);
        assert!(!opt.may_select("OPPO"));
        let Err(e) = parse(&["192.0.2.1", "--avoid-sources", "bluray,", "oppo"]) else {
            panic!("two addresses accepted");
        };
        assert!(e.contains("more than one receiver address"), "{e}");
    }

    #[test]
    fn list_options_refuse_bad_values() {
        for flag in ["--kinds", "--avoid-sources"] {
            let e = parse(&["192.0.2.1", flag]).err();
            assert!(
                e.is_some_and(|e| e.contains("needs names")),
                "{flag} missing"
            );
            let e = parse(&["192.0.2.1", flag, "--no-power"]).err();
            assert!(
                e.is_some_and(|e| e.contains("not the option")),
                "{flag} option"
            );
            for empty in ["", ",", " ", " , "] {
                let e = parse(&["192.0.2.1", flag, empty]).err();
                assert!(
                    e.is_some_and(|e| e.contains("no names")),
                    "{flag} {empty:?}"
                );
            }
            assert_eq!(
                Ok(names(&["a", "b"])),
                name_list(flag, Some("a,,b,".to_owned()))
            );
        }
    }

    #[test]
    fn base_steps_cannot_be_skipped() {
        for name in BASE_STEPS {
            let e = parse(&["192.0.2.1", "--skip", &format!("mute,{name}")]).err();
            let e = e.unwrap_or_else(|| panic!("{name} was skipped"));
            assert!(e.contains("cannot be skipped"), "{e}");
        }
        assert!(parse(&["192.0.2.1", "--skip", "metadata"]).is_ok());
    }

    #[test]
    fn skipped_end_steps_give_a_note() {
        assert_eq!(None, skip_note(&names(&["mute"])));
        assert_eq!(
            Some("restore_power and log always run".to_owned()),
            skip_note(&names(&["mute", "log"]))
        );
    }

    #[test]
    fn default_paths_are_in_the_repository() {
        let opt = parse(&["192.0.2.1"]).expect("valid");
        assert!(opt.bin.ends_with("target/release/uc-intg-denonmarantz"));
        assert!(opt.bin.starts_with(repo_root()));
        assert!(opt.commands.starts_with(repo_root()));
        assert!(repo_root().join("resources/commands.tsv").is_file());
    }

    #[test]
    fn without_sources_to_avoid_no_source_is_selected() {
        assert!(!options(&[]).may_select("Apple TV"));
        let opt = options(&["Oppo"]);
        assert!(opt.may_select("Apple TV"));
        assert!(!opt.may_select("OPPO Blu-ray"));
    }
}
