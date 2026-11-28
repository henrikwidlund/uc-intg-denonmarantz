// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Results of the parts of a step, joined into one result, and the empty value of the driver.

use crate::{Check, Fail};
use std::fmt::Write as _;

/// Empty value of a sensor or a select of the driver.
pub const EMPTY: &str = "--";

/// Result of a part of a step.
#[derive(Debug, PartialEq)]
pub enum Part {
    Pass(String),
    Skip(String),
    Fail(String),
}

impl From<Check> for Part {
    fn from(result: Check) -> Self {
        match result {
            Ok(note) => Self::Pass(note),
            Err(f) if f.skip => Self::Skip(f.msg),
            Err(f) => Self::Fail(f.msg),
        }
    }
}

/// One note for all parts. Fails if a part fails, skips only if all parts skip.
pub fn combine(parts: &[(&str, Part)]) -> Check {
    let mut note = String::new();
    for (name, part) in parts {
        if !note.is_empty() {
            note.push_str("; ");
        }
        let _ = match part {
            Part::Pass(msg) => write!(note, "{name}: {msg}"),
            Part::Skip(msg) => write!(note, "{name} skipped: {msg}"),
            Part::Fail(msg) => write!(note, "{name} failed: {msg}"),
        };
    }
    if parts.iter().any(|(_, p)| matches!(p, Part::Fail(_))) {
        Err(Fail::new(note))
    } else if parts.iter().all(|(_, p)| matches!(p, Part::Skip(_))) {
        Err(Fail::skip(note))
    } else {
        Ok(note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_are_combined() {
        let pass = || Part::Pass("ok".to_owned());
        let skip = || Part::Skip("why".to_owned());
        let fail = || Part::Fail("bad".to_owned());
        assert_eq!(
            Part::from(combine(&[("a", pass()), ("b", skip())])),
            Part::Pass("a: ok; b skipped: why".to_owned())
        );
        assert_eq!(
            Part::from(combine(&[("a", skip()), ("b", skip())])),
            Part::Skip("a skipped: why; b skipped: why".to_owned())
        );
        assert_eq!(
            Part::from(combine(&[("a", skip()), ("b", fail())])),
            Part::Fail("a skipped: why; b failed: bad".to_owned())
        );
        assert_eq!(
            Part::from(combine(&[("a", fail()), ("b", pass())])),
            Part::Fail("a failed: bad; b: ok".to_owned())
        );
    }

    #[test]
    fn part_from_check() {
        assert_eq!(Part::from(Ok("n".to_owned())), Part::Pass("n".to_owned()));
        assert_eq!(Part::from(Err(Fail::skip("s"))), Part::Skip("s".to_owned()));
        assert_eq!(Part::from(Err(Fail::new("f"))), Part::Fail("f".to_owned()));
    }
}
