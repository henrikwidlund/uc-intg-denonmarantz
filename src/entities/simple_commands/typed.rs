// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Simple commands that use the typed calls of the library. The typed calls check the state of the
//! receiver, the ranges and the routing (telnet or HTTP). A name that is not here is sent as raw
//! text from the table.

use denon_marantz::{Avr, Error, State};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;

/// What a command can use besides the receiver.
// The fields are read by the commands of the tables that need them.
#[allow(dead_code)]
pub struct Cx<'a> {
    pub is_denon: bool,
    /// The state of the receiver now. Toggles use it.
    pub state: &'a State,
}

pub type CallFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Error>> + 'a>>;

/// A command: it calls the library.
pub type Run = for<'a> fn(&'a Avr, &'a Cx<'a>) -> CallFuture<'a>;

/// One entry of a table: the name of the command and its call.
///
/// `call!("NAME", |avr, cx| avr.device().set_dimmer(DimmerMode::Bright))`
macro_rules! call {
    ($name:literal, |$avr:ident, $cx:ident| $body:expr) => {
        ($name, {
            #[allow(unused_variables)]
            fn run<'a>(
                $avr: &'a denon_marantz::Avr,
                $cx: &'a $crate::entities::simple_commands::Cx<'a>,
            ) -> $crate::entities::simple_commands::CallFuture<'a> {
                Box::pin(async move { $body.await })
            }
            run as $crate::entities::simple_commands::Run
        })
    };
}

mod group_a;
mod group_b;
mod group_c;
mod group_d;

type Table = &'static [(&'static str, Run)];

const TABLES: [Table; 4] = [
    group_a::TABLE,
    group_b::TABLE,
    group_c::TABLE,
    group_d::TABLE,
];

static REGISTRY: LazyLock<HashMap<&'static str, Run>> = LazyLock::new(|| {
    TABLES
        .iter()
        .flat_map(|table| table.iter().copied())
        .collect()
});

/// The typed call of a command, if there is one.
pub fn lookup(name: &str) -> Option<Run> {
    REGISTRY.get(name).copied()
}

/// The number of entries in all tables, also the ones with a name that is used twice.
#[cfg(test)]
pub fn entry_count() -> usize {
    TABLES.iter().map(|t| t.len()).sum()
}

/// The number of different names.
#[cfg(test)]
pub fn name_count() -> usize {
    REGISTRY.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::simple_commands::{ENTRIES, Entry};
    use crate::test_support::Fake;

    /// Texts that the library sends and that are not the text of the table. The library has the
    /// text of the specification, the table has a text of the Python integration.
    /// `(name, text of the library)`
    const LIBRARY_TEXT: &[(&str, &str)] = &[
        ("BASS_DOWN", "PSTONE CTRL ON + PSBAS DOWN"),
        ("BASS_UP", "PSTONE CTRL ON + PSBAS UP"),
        ("CENTER_WIDTH_DOWN", "PSCEN DOWN + PSCEW DOWN"),
        ("CENTER_WIDTH_UP", "PSCEN UP + PSCEW UP"),
        ("TREBLE_DOWN", "PSTONE CTRL ON + PSTRE DOWN"),
        ("TREBLE_UP", "PSTONE CTRL ON + PSTRE UP"),
    ];

    /// Commands that the fake receiver cannot run: it has no Dirac Live.
    const NOT_ON_FAKE: &[&str] = &[
        "DIRAC_LIVE_FILTER_OFF",
        "DIRAC_LIVE_FILTER_SLOT1",
        "DIRAC_LIVE_FILTER_SLOT2",
        "DIRAC_LIVE_FILTER_SLOT3",
    ];

    /// The command text of a request path: `%20` is a space.
    fn decode(path: &str) -> String {
        let mut out = String::new();
        let bytes = path.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%'
                && let Some(hex) = path.get(i + 1..i + 3)
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                out.push(char::from(byte));
                i += 3;
            } else {
                out.push(char::from(bytes[i]));
                i += 1;
            }
        }
        out
    }

    fn typed_entries() -> impl Iterator<Item = &'static Entry> {
        ENTRIES.iter().filter(|e| lookup(e.name).is_some())
    }

    #[test]
    fn a_name_is_in_one_table_only() {
        assert_eq!(entry_count(), name_count());
    }

    #[test]
    fn every_typed_name_is_in_the_table() {
        let unknown: Vec<_> = TABLES
            .iter()
            .flat_map(|t| t.iter().map(|(name, _)| *name))
            .filter(|name| !ENTRIES.iter().any(|e| e.name == *name))
            .collect();
        assert!(unknown.is_empty(), "not in commands.tsv: {unknown:?}");
    }

    /// Texts of the library for a Marantz receiver that are not the text of the table.
    const LIBRARY_TEXT_MARANTZ: &[(&str, &str)] = &[("PICTURE_MODE_CUSTOM", "PVCTM")];

    /// Runs the typed commands of a brand on a fake receiver and compares what they send with the
    /// text of the table. Returns the differences.
    async fn differences(denon: bool) -> Vec<String> {
        let fake = if denon {
            Fake::start(true).await
        } else {
            Fake::start_marantz().await
        };
        let avr = fake.avr().await;
        let state = avr.snapshot();
        let cx = Cx {
            is_denon: denon,
            state: &state,
        };
        let mut wrong = Vec::new();
        let entries = typed_entries().filter(|e| {
            if denon {
                e.for_denon()
            } else {
                e.for_marantz()
            }
        });
        for entry in entries {
            if NOT_ON_FAKE.contains(&entry.name) {
                continue;
            }
            let known = fake.commands().len();
            let run = lookup(entry.name).expect("typed");
            if let Err(e) = run(&avr, &cx).await {
                wrong.push(format!("{}: error {e}", entry.name));
                continue;
            }
            let sent: Vec<String> = fake.commands()[known..].iter().map(|p| decode(p)).collect();
            let brand_text = if denon { &[][..] } else { LIBRARY_TEXT_MARANTZ };
            let expected = brand_text
                .iter()
                .chain(LIBRARY_TEXT)
                .find(|(name, _)| *name == entry.name)
                .map_or_else(|| entry.text(denon, &state), |(_, text)| (*text).to_owned());
            if sent.join(" + ") != expected {
                wrong.push(format!("{}: sent {sent:?}, table {expected:?}", entry.name));
            }
        }
        wrong
    }

    #[tokio::test]
    async fn every_typed_command_sends_the_text_of_the_table_on_denon() {
        let wrong = differences(true).await;
        assert!(
            wrong.is_empty(),
            "{} differences:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    #[tokio::test]
    async fn every_typed_command_sends_the_text_of_the_table_on_marantz() {
        let wrong = differences(false).await;
        assert!(
            wrong.is_empty(),
            "{} differences:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    /// A toggle sends the other value when the receiver has one. The state is set with the lines
    /// of the receiver: the text of the toggle has both values, for example `PSGEQ ON/OFF`.
    #[tokio::test]
    async fn a_toggle_sends_the_opposite_of_the_current_value() {
        use crate::entities::simple_commands::toggle_current;
        use denon_marantz::proto::telnet::Message;
        let fake = Fake::start(true).await;
        let avr = fake.avr().await;
        let mut tested = 0;
        let mut wrong = Vec::new();
        for entry in typed_entries().filter(|e| e.for_denon() && e.name.ends_with("_TOGGLE")) {
            let Some((head, values)) = entry.text_for_test().rsplit_once(' ') else {
                continue;
            };
            if !values.contains('/') {
                continue;
            }
            for value in values.split('/') {
                let mut state = avr.snapshot();
                let line = format!("{head} {value}");
                let Some(message) = Message::parse(&line) else {
                    continue;
                };
                state.apply_message(&message);
                if toggle_current(entry.name, &state).is_none() {
                    continue;
                }
                let cx = Cx {
                    is_denon: true,
                    state: &state,
                };
                let known = fake.commands().len();
                let run = lookup(entry.name).expect("typed");
                if let Err(e) = run(&avr, &cx).await {
                    wrong.push(format!("{} after {line}: error {e}", entry.name));
                    continue;
                }
                let sent: Vec<String> =
                    fake.commands()[known..].iter().map(|p| decode(p)).collect();
                let expected = entry.text(true, &state);
                // Some commands send the tone control on first.
                if sent.last().map(String::as_str) != Some(expected.as_str()) {
                    wrong.push(format!(
                        "{} after {line}: sent {sent:?}, table {expected:?}",
                        entry.name
                    ));
                }
                tested += 1;
            }
        }
        assert!(
            wrong.is_empty(),
            "{} differences:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
        assert!(tested >= 20, "only {tested} toggle states tested");
    }

    /// Runs every typed command on a receiver in a state and lists the ones that the library
    /// refuses because of the state. Nothing is sent for them.
    async fn refused_in(lines: &'static [&'static str]) -> Vec<&'static str> {
        use crate::device::discovery::{TEST_BUILDER, TestBuilder, start};
        use crate::test_support::{FakeTelnet, test_builder};
        use std::rc::Rc;
        let http = Fake::start(true).await;
        let telnet = FakeTelnet::start(move |line| {
            if line.ends_with('?') {
                lines.iter().map(|l| (*l).to_owned()).collect()
            } else {
                Vec::new()
            }
        })
        .await;
        let (http_port, telnet_port) = (http.port(), telnet.port);
        let setup: TestBuilder =
            Rc::new(move |_, builder| test_builder(http_port, telnet_port, builder));
        TEST_BUILDER.with(|slot| *slot.borrow_mut() = Some(setup));
        let avr = std::sync::Arc::new(
            test_builder(http_port, telnet_port, Avr::builder("127.0.0.1"))
                .connect()
                .await
                .expect("connect"),
        );
        start(&avr, true).await.expect("start");
        // The answers of the fake arrive as events.
        for _ in 0..100 {
            if avr.snapshot().device().dirac_filter.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let state = avr.snapshot();
        let cx = Cx {
            is_denon: true,
            state: &state,
        };
        let mut refused = Vec::new();
        for entry in typed_entries().filter(|e| e.for_denon()) {
            if NOT_ON_FAKE.contains(&entry.name) {
                continue;
            }
            if let Err(e) = lookup(entry.name).expect("typed")(&avr, &cx).await
                && e.is_blocked()
            {
                refused.push(entry.name);
            }
        }
        refused
    }

    /// With a Dirac filter selected, the library refuses the tone control, the Audyssey settings
    /// and the reference level. With Dynamic EQ off it refuses the reference level.
    #[tokio::test]
    async fn the_commands_that_the_library_refuses_in_a_state() {
        let dirac = refused_in(&["PSDIRAC 2", "PSDYNEQ ON"]).await;
        println!(
            "{} refused with a Dirac filter:\n{}",
            dirac.len(),
            dirac.join("\n")
        );
        let no_dynamic_eq = refused_in(&["PSDIRAC OFF", "PSDYNEQ OFF"]).await;
        println!(
            "{} refused with Dynamic EQ off:\n{}",
            no_dynamic_eq.len(),
            no_dynamic_eq.join("\n")
        );
        assert!(!dirac.is_empty() && !no_dynamic_eq.is_empty());
    }

    /// Lists the commands that are sent as raw text. Run it with `--nocapture`.
    #[test]
    fn the_commands_without_a_typed_call() {
        let raw: Vec<&str> = ENTRIES
            .iter()
            .filter(|e| lookup(e.name).is_none())
            .map(|e| e.name)
            .collect();
        println!(
            "{} of {} commands are sent as raw text:",
            raw.len(),
            ENTRIES.len()
        );
        println!("{}", raw.join("\n"));
    }
}
