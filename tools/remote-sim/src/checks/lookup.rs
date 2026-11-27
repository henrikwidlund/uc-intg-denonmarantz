// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! How the driver finds the receiver when the stored address does not answer.

use super::{
    UNUSED_ADDRESS, config_back, config_device, connect_ms, first_error, log_after, read_config,
    restart, wait_available,
};
use crate::{Check, Ctx, Fail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Longest connect time in the driver log when the driver must search the network for the
/// receiver.
const MAX_SEARCH_MS: u64 = 20_000;

/// Part of the driver log line when the stored address changed after a connect.
const ADDRESS_LINE: &str = "] address changed: ";

/// Part of the driver log line for a full connect.
const CONNECTED_LINE: &str = "] connected in ";

/// The stored address does not answer: the driver must find the receiver by the host name of
/// `--host-name` (part "host name") and by a search for its MAC address or serial number (part
/// "search"). In the search part, a dimmer command with the current option is sent while the
/// connect still runs: it must wait for the connect and work. At the end the saved `config.json`
/// is written back and the driver is started again.
pub async fn address_lookup(ctx: &mut Ctx) -> Check {
    let good = tokio::fs::read_to_string(ctx.driver.config_file()).await?;
    let config = read_config(ctx).await?;
    let address = config_device(ctx, &config)?["address"]
        .as_str()
        .filter(|a| !a.is_empty() && *a != UNUSED_ADDRESS)
        .ok_or_else(|| Fail::new("config.json has no address"))?
        .to_owned();
    let dimmer = ctx.select("dimmer");
    let option =
        Some(ctx.client.text(&dimmer, "current_option")).filter(|o| !o.is_empty() && o != "--");
    let result = async {
        let host = match ctx.opt.host_name.clone() {
            Some(host) => host_part(ctx, &config, &address, &host).await,
            None => Err(Fail::skip("no --host-name")),
        };
        let search = search_part(ctx, &config, &address, option.as_deref()).await;
        combine([("host name", host), ("search", search)])
    }
    .await;
    let undo = config_back(ctx, good).await;
    first_error(result, undo)
}

/// The driver must connect with the addresses of `host` and store the address again.
async fn host_part(ctx: &mut Ctx, config: &Value, address: &str, host: &str) -> Check {
    let ips: Vec<String> = tokio::net::lookup_host((host, 80))
        .await
        .map_err(|e| Fail::skip(format!("{host} not resolved here: {e}")))?
        .map(|a| a.ip().to_string())
        .collect();
    if !ips.iter().any(|ip| ip == address) {
        return Err(Fail::skip(format!(
            "{host} resolves to {ips:?}, not to {address}"
        )));
    }
    let from = start(ctx, lookup_config(config, &ctx.key, Some(host))?).await?;
    let ms = connected(ctx, from, address, Some(host)).await?;
    let log = log_after(ctx, from).await;
    if !log.contains(&format!("] host name {host} resolved to ")) {
        return Err(Fail::new("no host name line in the driver log"));
    }
    Ok(format!("connected in {ms} ms"))
}

/// The driver must find the receiver with a search, store the address again and keep no host
/// name. A dimmer command sent during the connect must work.
async fn search_part(ctx: &mut Ctx, config: &Value, address: &str, option: Option<&str>) -> Check {
    let from = start(ctx, lookup_config(config, &ctx.key, None)?).await?;
    let command = command_during_connect(ctx, from, option).await;
    let found = connected(ctx, from, address, None).await;
    let log = log_after(ctx, from).await;
    let ms = found?;
    if log.contains("] host name ") {
        return Err(Fail::new("the driver used a host name, but none is stored"));
    }
    if ms > MAX_SEARCH_MS {
        return Err(Fail::new(format!(
            "connected in {ms} ms, more than {MAX_SEARCH_MS} ms"
        )));
    }
    match command {
        Ok(note) => Ok(format!("connected in {ms} ms, {note}")),
        Err(e) if e.skip => Ok(format!("connected in {ms} ms, command skipped: {e}")),
        Err(e) => Err(Fail::new(format!("connected in {ms} ms, command: {e}"))),
    }
}

/// Send the current dimmer option again while the driver still connects. The answer must be OK
/// and come in less than 10 s.
async fn command_during_connect(ctx: &mut Ctx, from: u64, option: Option<&str>) -> Check {
    let option = option.ok_or_else(|| Fail::skip("no dimmer option"))?;
    if log_after(ctx, from).await.contains(CONNECTED_LINE) {
        return Err(Fail::skip("the connect ended before the command"));
    }
    let dimmer = ctx.select("dimmer");
    let start = Instant::now();
    ctx.client
        .command_ok(
            "select",
            &dimmer,
            "select_option",
            json!({ "option": option }),
        )
        .await?;
    Ok(format!(
        "dimmer {option} sent during the connect, answer in {} ms",
        start.elapsed().as_millis()
    ))
}

/// Stop the driver, write `config` and start the driver again with a new Remote. Gives the log
/// size before the start.
async fn start(ctx: &mut Ctx, config: Value) -> Result<u64, Fail> {
    ctx.driver.stop().await;
    tokio::fs::write(ctx.driver.config_file(), config.to_string()).await?;
    let from = ctx.driver.log_size().await;
    restart(ctx).await?;
    ctx.client.refresh_states().await?;
    Ok(from)
}

/// Wait for the connect, then check the driver log and `config.json`: the stored address must be
/// `address` again and the host name must be `host`. Gives the connect time from the log.
async fn connected(
    ctx: &mut Ctx,
    from: u64,
    address: &str,
    host: Option<&str>,
) -> Result<u64, Fail> {
    wait_available(ctx, 30).await?;
    let log = log_after(ctx, from).await;
    let ms = connect_ms(&log).ok_or_else(|| Fail::new("no connect line in the driver log"))?;
    if !log.contains(&format!("{ADDRESS_LINE}{UNUSED_ADDRESS} -> {address}")) {
        return Err(Fail::new(format!(
            "no address change to {address} in the driver log"
        )));
    }
    // The controller saves the new address after the connect.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let config = read_config(ctx).await?;
        let device = config_device(ctx, &config)?;
        let stored = (device["address"].as_str(), device["host"].as_str());
        if stored == (Some(address), host) {
            return Ok(ms);
        }
        if Instant::now() >= deadline {
            return Err(Fail::new(format!(
                "config.json after 5 s: address {}, host {}",
                device["address"], device["host"]
            )));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

/// `config` with the documentation address [`UNUSED_ADDRESS`] for the receiver with the id `key`,
/// and with `host` as its host name. Without `host`, the host name is removed.
fn lookup_config(config: &Value, key: &str, host: Option<&str>) -> Result<Value, Fail> {
    let mut config = config.clone();
    let device = config
        .as_array_mut()
        .and_then(|a| a.iter_mut().find(|d| d["id"] == key))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Fail::new("the receiver is not in config.json"))?;
    device.insert("address".to_owned(), json!(UNUSED_ADDRESS));
    match host {
        Some(host) => device.insert("host".to_owned(), json!(host)),
        None => device.remove("host"),
    };
    Ok(config)
}

/// Join the results of the parts. A skipped part is a note. The check fails if a part fails.
fn combine<const N: usize>(parts: [(&str, Check); N]) -> Check {
    let mut notes = Vec::with_capacity(N);
    let mut failed = false;
    for (name, result) in parts {
        match result {
            Ok(note) => notes.push(format!("{name}: {note}")),
            Err(e) if e.skip => notes.push(format!("{name} skipped: {e}")),
            Err(e) => {
                failed = true;
                notes.push(format!("{name} FAILED: {e}"));
            }
        }
    }
    let text = notes.join("; ");
    if failed {
        Err(Fail::new(text))
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Value {
        json!([
            {"id": "A", "address": "192.0.2.6", "host": "avr.local"},
            {"id": "B", "address": "192.0.2.7"}
        ])
    }

    #[test]
    fn lookup_config_sets_address_and_host() {
        let with = lookup_config(&config(), "A", Some("denon"))
            .ok()
            .expect("found");
        assert_eq!(with[0]["address"], UNUSED_ADDRESS);
        assert_eq!(with[0]["host"], "denon");
        assert_eq!(with[1], config()[1], "other receiver unchanged");
        let without = lookup_config(&config(), "A", None).ok().expect("found");
        assert_eq!(without[0]["address"], UNUSED_ADDRESS);
        assert!(without[0].get("host").is_none(), "{without}");
        assert!(lookup_config(&config(), "C", None).is_err());
        assert!(lookup_config(&json!({}), "A", None).is_err());
    }

    #[test]
    fn combine_fails_only_on_a_failed_part() {
        let ok = combine([("a", Ok("x".to_owned())), ("b", Err(Fail::skip("y")))]);
        assert_eq!(ok.ok().as_deref(), Some("a: x; b skipped: y"));
        let Err(e) = combine([("a", Err(Fail::new("x"))), ("b", Ok("y".to_owned()))]) else {
            panic!("a failed part fails");
        };
        assert!(!e.skip);
        assert_eq!(e.msg, "a FAILED: x; b: y");
    }
}
