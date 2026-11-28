// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Questions to the owner on the terminal.

use super::{above_baseline, lower_volume, receiver_volume, unmute_limit};
use crate::oracle::Oracle;
use crate::{Ctx, Fail};
use std::time::Duration;

/// Lines of the terminal. One thread reads them, so no typed line is lost between two reads.
/// The channel closes at the end of the input.
pub fn stdin_lines() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// True if stdin is a terminal, so the owner can answer.
pub fn terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Throw away the lines typed before a prompt, so they do not answer it.
fn drop_typed_lines(ctx: &mut Ctx) {
    if let Some(rx) = ctx.stdin.as_mut() {
        while rx.try_recv().is_ok() {}
    }
}

/// Read a line from the terminal. `None` at the end of the input. The WebSocket is read
/// meanwhile, so a closed connection fails the check and the states stay current.
pub(crate) async fn read_line(ctx: &mut Ctx) -> Result<Option<String>, Fail> {
    let rx = ctx.stdin.get_or_insert_with(stdin_lines);
    loop {
        tokio::select! {
            line = rx.recv() => return Ok(line),
            result = ctx.client.listen_ms(500) => result?,
        }
    }
}

/// What the owner answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Answer {
    Done,
    Yes,
    No,
    Skip,
    /// The input ended, so the owner cannot answer.
    NoInput,
}

/// Kind of question to the owner.
#[derive(Clone, Copy)]
pub(crate) enum Ask {
    /// Something to do. Enter means it is done, s skips. The texts say what each key means.
    Action(&'static str, &'static str),
    /// A yes or no question. The texts say what y, n and s mean.
    Question(&'static str, &'static str, &'static str),
}

/// What s + Enter means for most prompts.
pub(crate) const SKIP_CHECK: &str = "skip this check";

/// The line that says what each key does.
fn keys_text(kind: Ask) -> String {
    match kind {
        Ask::Action(done, skip) => format!("Enter = {done}, s + Enter = {skip}"),
        Ask::Question(yes, no, skip) => {
            format!("y + Enter = {yes}, n + Enter = {no}, s + Enter = {skip}")
        }
    }
}

/// The answer for a typed line. `None` if the line is not a valid answer for `kind`.
fn parse_answer(kind: Ask, line: &str) -> Option<Answer> {
    match (kind, line.trim().to_ascii_lowercase().as_str()) {
        (_, "s") => Some(Answer::Skip),
        (Ask::Action(..), "") => Some(Answer::Done),
        (Ask::Question(..), "y") => Some(Answer::Yes),
        (Ask::Question(..), "n") => Some(Answer::No),
        _ => None,
    }
}

/// Ask the owner on the terminal and wait for the answer. Without a terminal it returns `Skip`,
/// at the end of the input `NoInput`. The question and the answer go to the report of the step.
pub(crate) async fn ask(ctx: &mut Ctx, kind: Ask, text: &str) -> Result<Answer, Fail> {
    ask_hint(ctx, kind, text, None).await
}

/// [`ask`] with a `hint` line under the question. The report gets only the question.
pub(crate) async fn ask_hint(
    ctx: &mut Ctx,
    kind: Ask,
    text: &str,
    hint: Option<&str>,
) -> Result<Answer, Fail> {
    if !terminal() {
        ctx.answers
            .push(format!("{text:?}: not asked, no terminal"));
        return Ok(Answer::Skip);
    }
    let keys = keys_text(kind);
    drop_typed_lines(ctx);
    match hint {
        Some(hint) => println!("\n  >>> {text}\n  {hint}\n  {keys}"),
        None => println!("\n  >>> {text}\n  {keys}"),
    }
    let answer = loop {
        let Some(line) = read_line(ctx).await? else {
            break Answer::NoInput;
        };
        match parse_answer(kind, &line) {
            Some(answer) => break answer,
            None => println!("  Unknown answer. {keys}"),
        }
    };
    ctx.answers.push(format!("{text:?}: {answer:?}"));
    Ok(answer)
}

/// Milliseconds between two reads of the receiver volume while a check watches the volume.
pub(crate) const WATCH_MS: u64 = 1000;

/// The end of the text of a [`Raised`] that was not lowered.
pub(crate) const STOP_TEXT: &str = "stop: no more sound prompts";

/// The text `msg` of a check result in which each part between `; ` that is exactly [`STOP_TEXT`]
/// is moved to the end, once. Joined failures of one check can each end with it. A part that only
/// ends with it stays as it is.
pub fn stop_text_once(msg: String) -> String {
    let parts: Vec<&str> = msg.split("; ").collect();
    if !parts.contains(&STOP_TEXT) {
        return msg;
    }
    let mut out: Vec<&str> = parts.into_iter().filter(|p| *p != STOP_TEXT).collect();
    out.push(STOP_TEXT);
    out.join("; ")
}

/// A receiver volume above the start volume or above [`MAX_VOLUME`](super::MAX_VOLUME), or a
/// volume read or lowering that failed. All are failures.
pub(crate) struct Raised {
    /// What happened and what was done.
    pub(crate) fail: Fail,
    /// The volume was lowered. False if the read or the lowering failed: the volume can still be
    /// above the start volume, so no more sound prompts and no sound source selects follow in the
    /// step. The source set back then goes through the owner.
    pub(crate) lowered: bool,
}

impl Raised {
    /// A rise with the text `text`. Without `lowered`, [`STOP_TEXT`] is added to the text if it
    /// does not end with it.
    pub(crate) fn new(mut text: String, lowered: bool) -> Self {
        if !lowered && !text.ends_with(STOP_TEXT) {
            text.push_str("; ");
            text.push_str(STOP_TEXT);
        }
        Self {
            fail: Fail::new(text),
            lowered,
        }
    }

    /// Add `text` to the text, before [`STOP_TEXT`] if the rise was not lowered.
    pub(crate) fn add(&mut self, text: &str) {
        let msg = &mut self.fail.msg;
        let stop = msg
            .strip_suffix(STOP_TEXT)
            .map(str::len)
            .filter(|_| !self.lowered);
        match stop {
            Some(at) => msg.insert_str(at, &format!("{text}; ")),
            None => {
                msg.push_str("; ");
                msg.push_str(text);
            }
        }
    }

    /// A failed volume read `e`. `when` says when the volume was read.
    fn read_failed(when: &str, e: &Fail) -> Self {
        Self::new(
            format!("the receiver volume {when} could not be read: {e}"),
            false,
        )
    }
}

/// The failure of `rise`. Sets `stop` if it was not lowered: no more sound prompts follow.
pub(crate) fn stop_fail(stop: &mut bool, rise: Raised) -> Fail {
    *stop |= !rise.lowered;
    rise.fail
}

/// `Ok` without a rise, else the failure of `rise`.
pub(crate) fn rise_result(rise: Option<Raised>) -> Result<(), Fail> {
    rise.map_or(Ok(()), |rise| Err(rise.fail))
}

/// The text of an `Ok` value that [`add_failure`] keeps. Empty by default.
pub(crate) trait Note {
    fn note(&self) -> &str {
        ""
    }
}

impl Note for String {
    fn note(&self) -> &str {
        self
    }
}

impl Note for () {}

/// Add the result `failure` of a volume check or a set back to `result`, for example a rise from
/// [`rise_result`]. A failure is always a failure, also after a skip or with a skip as `failure`.
/// The note of an `Ok` value comes first: `"{note}; {failure}"`.
pub(crate) fn add_failure<T: Note>(
    result: Result<T, Fail>,
    failure: Result<(), Fail>,
) -> Result<T, Fail> {
    let Err(failure) = failure else {
        return result;
    };
    let note = match &result {
        Ok(value) => value.note(),
        Err(e) => e.msg.as_str(),
    };
    Err(if note.is_empty() {
        Fail::new(failure.msg)
    } else {
        Fail::new(format!("{note}; {failure}"))
    })
}

/// What a wait of [`watched`] gave.
pub(crate) enum Watched<T = Answer> {
    /// The wait ended by itself, with this value, and the volume read right after it was not above
    /// the start volume.
    Done(T),
    /// The wait ended by itself with this value, and right after it the receiver volume was above
    /// the start volume and a lowering was tried, or the volume read failed.
    After(T, Raised),
    /// The wait ended early: the receiver volume went above the start volume and a lowering was
    /// tried, or the volume read failed. Also when the wait failed and the volume read right after
    /// it found such a rise: the text of the rise then has the error of the wait.
    Ended(Raised),
}

/// Run `wait` and read the receiver volume on a copy of the second connection about every
/// [`WATCH_MS`] ms while it runs: the owner or a phone can set the volume. A volume above `start`
/// ends the wait: it is lowered with [`lower_after_rise`]. A failed read also ends it. A rise or a
/// failed read wins over a result of `wait` that comes in the same poll. When `wait` ends by
/// itself, also with an error, [`rise_back`] runs once, so the caller needs no read after it: a
/// rise there gives [`Watched::After`]. `when` says in the text when the volume was read. Every
/// rise is noted for the step, see [`note_rise`]. Error if `wait` fails and the volume is not above
/// `start`. Skips without a second connection. Error after a rise that was not lowered in the step,
/// see [`sound_allowed`]. In both cases `wait` does not run. When the watch ends the wait, a frame
/// that the wait left in the write buffer of the WebSocket is sent at once, before the lowering.
pub(crate) async fn watched<T>(
    ctx: &mut Ctx,
    start: f64,
    when: &str,
    wait: impl AsyncFnOnce(&mut Ctx) -> Result<T, Fail>,
) -> Result<Watched<T>, Fail> {
    sound_allowed(ctx)?;
    watched_any(ctx, start, when, wait).await
}

/// [`watched`] without the check of [`sound_allowed`] for the stop. For the waits that make the
/// receiver safe again, such as a set back or a pause. Skips without a second connection.
pub(crate) async fn watched_any<T>(
    ctx: &mut Ctx,
    start: f64,
    when: &str,
    wait: impl AsyncFnOnce(&mut Ctx) -> Result<T, Fail>,
) -> Result<Watched<T>, Fail> {
    // The copy shares the connection, so `wait` can use `ctx.oracle` meanwhile.
    let Some(oracle) = ctx.oracle.clone() else {
        return Err(Fail::skip(format!("not run: {NO_WATCH}")));
    };
    let result = tokio::select! {
        biased;
        rise = watch_volume(&oracle, start) => Err(rise),
        done = wait(ctx) => Ok(done),
    };
    if result.is_err() {
        // A failed flush is not checked here: the next read of the WebSocket fails then.
        let _ = tokio::time::timeout(FLUSH_LIMIT, ctx.client.flush()).await;
    }
    let rise = match result {
        Ok(done) => {
            return match (
                rise_back(ctx, start, &format!("right after: {when}")).await,
                done,
            ) {
                (None, done) => done.map(Watched::Done),
                (Some(rise), Ok(value)) => Ok(Watched::After(value, rise)),
                (Some(mut rise), Err(e)) => {
                    rise.add(&format!("the wait failed: {e}"));
                    Ok(Watched::Ended(rise))
                }
            };
        }
        Err(Ok(now)) => lower_after_rise(ctx, now, start, when).await,
        Err(Err(e)) => note_rise(ctx, Raised::read_failed(when, &e)),
    };
    Ok(Watched::Ended(rise))
}

/// The skip of [`watched`] without a second connection.
pub(crate) const NO_WATCH: &str = "no second connection: the volume cannot be watched";

/// Error with [`STOP_TEXT`] after a rise that was not lowered in the step, see [`note_rise`]: no
/// more sound prompts, sound commands or source selects of the check follow. Only the owner may
/// set the source back then.
pub(crate) fn sound_allowed(ctx: &Ctx) -> Result<(), Fail> {
    if ctx.rises.stop {
        return Err(Fail::new(format!("not run: {STOP_TEXT}")));
    }
    Ok(())
}

/// Note the rise `rise` for the step and return it. The step adds the text of each noted rise
/// that its result does not hold, so a later error cannot drop a rise. A rise that was not
/// lowered sets the stop of [`sound_allowed`].
pub(crate) fn note_rise(ctx: &mut Ctx, rise: Raised) -> Raised {
    let msg = rise.fail.msg.as_str();
    let text = msg
        .strip_suffix(STOP_TEXT)
        .map_or(msg, |m| m.trim_end_matches("; "));
    ctx.rises.texts.push(text.to_owned());
    ctx.rises.stop |= !rise.lowered;
    rise
}

/// The rises of the current step, see [`note_rise`].
#[derive(Default)]
pub struct Rises {
    /// The text of each rise, without [`STOP_TEXT`].
    pub texts: Vec<String>,
    /// A rise was not lowered.
    pub stop: bool,
}

impl Rises {
    /// The step result `msg` with the text of each rise that it does not hold, and [`STOP_TEXT`]
    /// after a stop. `None` if nothing is missing.
    pub fn missing(&self, msg: &str) -> Option<String> {
        if self.texts.is_empty() && !self.stop {
            return None;
        }
        let mut out = msg.to_owned();
        for text in &self.texts {
            if out.contains(text.as_str()) {
                continue;
            }
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(text);
        }
        if self.stop && !out.contains(STOP_TEXT) {
            out.push_str("; ");
            out.push_str(STOP_TEXT);
        }
        (out != msg).then_some(out)
    }
}

/// The longest wait of [`watched`] for the flush of the WebSocket.
const FLUSH_LIMIT: Duration = Duration::from_secs(2);

/// Ask a prompt with [`ask_hint`] and watch the receiver volume with [`watched`] while it waits. A
/// volume above `start` or a failed read ends the prompt. Error if the prompt fails. Not asked
/// after a stop, see [`sound_allowed`]. Skips without a second connection: the prompt is not asked.
pub(crate) async fn ask_watched(
    ctx: &mut Ctx,
    kind: Ask,
    text: &str,
    hint: Option<&str>,
    start: f64,
) -> Result<Watched, Fail> {
    sound_allowed(ctx)?;
    owner_ask_watched(ctx, kind, text, hint, start).await
}

/// [`ask_watched`] also after a stop: for the owner prompts that make the receiver safe again.
/// Skips without a second connection: the prompt is not asked.
pub(crate) async fn owner_ask_watched(
    ctx: &mut Ctx,
    kind: Ask,
    text: &str,
    hint: Option<&str>,
    start: f64,
) -> Result<Watched, Fail> {
    let watched = watched_any(
        ctx,
        start,
        "while the prompt waited",
        async |ctx: &mut Ctx| ask_hint(ctx, kind, text, hint).await,
    )
    .await?;
    if let Watched::Ended(rise) = &watched {
        println!("  The volume went above the start volume or could not be read, the prompt ends.");
        ctx.answers
            .push(format!("{text:?}: not answered: {}", rise.fail));
    }
    Ok(watched)
}

/// Read the receiver volume on the second connection `oracle` [`WATCH_MS`] ms after the end of the
/// last read until it is above `start`. Returns that volume. Error if a read fails.
async fn watch_volume(oracle: &Oracle, start: f64) -> Result<f64, Fail> {
    loop {
        tokio::time::sleep(Duration::from_millis(WATCH_MS)).await;
        let now = oracle.fresh_volume().await?;
        if above_baseline(now, Some(start)) {
            return Ok(now);
        }
    }
}

/// Set a rise of the receiver volume back to `start`, the volume before the sound: read the
/// receiver volume and lower a volume above `start` with [`lower_after_rise`]. `None` if the
/// volume is not above `start`. A failed read is a [`Raised`] that was not lowered, and so is a
/// missing second connection: the volume is then not known. `when` says in the text when the
/// volume was read. The rise is noted, see [`note_rise`].
pub(crate) async fn rise_back(ctx: &mut Ctx, start: f64, when: &str) -> Option<Raised> {
    let rise = match receiver_volume(ctx).await {
        Ok(Some(now)) if above_baseline(now, Some(start)) => {
            return Some(lower_after_rise(ctx, now, start, when).await);
        }
        Ok(Some(_)) => return None,
        Ok(None) => Raised::read_failed(when, &Fail::new("no second connection")),
        Err(e) => Raised::read_failed(when, &e),
    };
    Some(note_rise(ctx, rise))
}

/// The result of a wait `waited` of [`watched`], and the rise. A wait that ended early gives the
/// error [`ENDED_EARLY`].
pub(crate) fn end_watch<T>(waited: Result<Watched<T>, Fail>) -> (Result<T, Fail>, Option<Raised>) {
    match waited {
        Ok(Watched::Done(value)) => (Ok(value), None),
        Ok(Watched::After(value, rise)) => (Ok(value), Some(rise)),
        Ok(Watched::Ended(rise)) => (Err(Fail::new(ENDED_EARLY)), Some(rise)),
        Err(e) => (Err(e), None),
    }
}

/// The error of a wait that [`watched`] ended early. The rise says why.
pub(crate) const ENDED_EARLY: &str = "stopped by the volume watch";

/// Read the events for `ms` ms with [`watched_any`]. Returns the result of the read and the rise.
/// A read that the watch ended is `Ok`. Skips without a second connection. `when` says in the text
/// when the volume was read.
pub(crate) async fn listen_watched(
    ctx: &mut Ctx,
    ms: u64,
    start: f64,
    when: &str,
) -> (Result<(), Fail>, Option<Raised>) {
    let waited = watched_any(ctx, start, when, async |ctx: &mut Ctx| {
        ctx.client.listen_ms(ms).await
    })
    .await;
    match waited {
        Ok(Watched::Ended(rise)) => (Ok(()), Some(rise)),
        waited => end_watch(waited),
    }
}

/// The rise `rise` was not lowered: no more sound prompts and no sound source selects follow.
pub(crate) fn stops(rise: &Option<Raised>) -> bool {
    rise.as_ref().is_some_and(|rise| !rise.lowered)
}

/// Lower the receiver volume `now`, which is above `start`, with [`lower_volume`]. The text says
/// what was done, see [`rise_text`]. `when` says when the volume was read.
async fn lower_after_rise(ctx: &mut Ctx, now: f64, start: f64, when: &str) -> Raised {
    let lowered = lower_volume(ctx, Some(start)).await;
    let rise = Raised::new(rise_text(now, start, when, &lowered), lowered.is_ok());
    note_rise(ctx, rise)
}

/// The text for a receiver volume `now` above `start`. `lowered` is the result of
/// [`lower_volume`]. `when` says when the volume was read.
pub(crate) fn rise_text(
    now: f64,
    start: f64,
    when: &str,
    lowered: &Result<Option<f64>, Fail>,
) -> String {
    let limit = unmute_limit(Some(start));
    let back = match lowered {
        Ok(Some(sent)) => format!("set back to {sent}"),
        Ok(None) => format!("not sent: the receiver is not above {limit} now"),
        Err(e) => format!("not set back to {limit}: {e}"),
    };
    format!("the receiver volume {now} {when} is above the start volume {start}, {back}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::MAX_VOLUME;

    const ACTION: Ask = Ask::Action("done", SKIP_CHECK);
    const QUESTION: Ask = Ask::Question("yes", "no", SKIP_CHECK);

    #[test]
    fn answers() {
        assert_eq!(Some(Answer::Done), parse_answer(ACTION, ""));
        assert_eq!(Some(Answer::Done), parse_answer(ACTION, "  "));
        assert_eq!(Some(Answer::Skip), parse_answer(ACTION, "S"));
        assert_eq!(None, parse_answer(ACTION, "y"));
        assert_eq!(Some(Answer::Yes), parse_answer(QUESTION, " Y "));
        assert_eq!(Some(Answer::No), parse_answer(QUESTION, "n"));
        assert_eq!(Some(Answer::Skip), parse_answer(QUESTION, "s"));
        assert_eq!(None, parse_answer(QUESTION, ""));
        assert_eq!(None, parse_answer(QUESTION, "yes"));
    }

    #[test]
    fn rise_text_says_what_was_done() {
        assert_eq!(
            rise_text(35.0, 30.0, "after Bluetooth", &Ok(Some(30.0))),
            "the receiver volume 35 after Bluetooth is above the start volume 30, set back to 30"
        );
        assert_eq!(
            rise_text(35.0, 30.0, "while the prompt waited", &Ok(None)),
            "the receiver volume 35 while the prompt waited is above the start volume 30, not sent: the receiver is not above 30 now"
        );
        let max = MAX_VOLUME;
        assert_eq!(
            rise_text(max + 5.0, max + 2.0, "x", &Err(Fail::new("e"))),
            format!(
                "the receiver volume {} x is above the start volume {}, not set back to {max}: e",
                max + 5.0,
                max + 2.0
            ),
            "the limit is never above the cap"
        );
    }

    #[test]
    fn a_noted_rise_is_never_dropped() {
        let rises = Rises {
            texts: vec!["a".to_owned(), "b".to_owned()],
            stop: false,
        };
        assert_eq!(None, rises.missing("x; a; b"), "the result has them");
        assert_eq!(Some("x; a; b".to_owned()), rises.missing("x; a"));
        assert_eq!(Some("a; b".to_owned()), rises.missing(""), "an empty note");
        let stop = Rises {
            texts: vec!["a".to_owned()],
            stop: true,
        };
        assert_eq!(Some(format!("e; a; {STOP_TEXT}")), stop.missing("e"));
        assert_eq!(None, stop.missing(&format!("a; {STOP_TEXT}")));
        assert_eq!(None, Rises::default().missing("x"));
        let twice = Rises {
            texts: vec!["a".to_owned(), "a".to_owned()],
            stop: false,
        };
        assert_eq!(
            Some("x; a".to_owned()),
            twice.missing("x"),
            "a text is added once"
        );
    }

    #[test]
    fn a_rise_that_was_not_lowered_stops_the_sound() {
        let lowered = Raised::new("rise".to_owned(), true);
        assert_eq!((lowered.fail.msg.as_str(), lowered.lowered), ("rise", true));
        let stop = Raised::new("rise".to_owned(), false);
        assert_eq!(
            (stop.fail.msg.as_str(), stop.lowered),
            ("rise; stop: no more sound prompts", false)
        );
        let read = Raised::read_failed("after the answer", &Fail::new("e"));
        assert_eq!(
            read.fail.msg,
            "the receiver volume after the answer could not be read: e; stop: no more sound prompts"
        );
        assert!(!read.lowered);
        assert!(rise_result(None).is_ok());
        assert!(!stops(&None));
        let mut added = Raised::new("v".to_owned(), false);
        added.add("t");
        assert_eq!(
            format!("v; t; {STOP_TEXT}"),
            added.fail.msg,
            "before the stop text"
        );
        let mut added = Raised::new("v".to_owned(), true);
        added.add("t");
        assert_eq!("v; t", added.fail.msg);
        assert_eq!(
            format!("a; b; {STOP_TEXT}"),
            stop_text_once(format!("a; {STOP_TEXT}; b; {STOP_TEXT}"))
        );
        assert_eq!("a; b", stop_text_once("a; b".to_owned()));
        let twice = Raised::new(Raised::new("v".to_owned(), false).fail.msg, false);
        assert_eq!(
            format!("v; {STOP_TEXT}"),
            twice.fail.msg,
            "the stop text once"
        );
        assert!(!stops(&Some(Raised::new("v".to_owned(), true))));
        assert!(stops(&Some(Raised::new("v".to_owned(), false))));
        assert_eq!(
            rise_result(Some(Raised::new("v".to_owned(), true)))
                .unwrap_err()
                .msg,
            "v"
        );
    }

    #[test]
    fn add_failure_keeps_the_note_and_fails() {
        let rise = || rise_result(Some(Raised::new("v".to_owned(), true)));
        assert_eq!(
            add_failure(Ok::<_, Fail>("a".to_owned()), Ok(())).ok(),
            Some("a".to_owned())
        );
        let e = add_failure(Err::<(), _>(Fail::skip("a")), Ok(())).unwrap_err();
        assert!(e.skip, "a skip without a failure stays a skip");
        let e = add_failure(Ok("a".to_owned()), rise()).unwrap_err();
        assert_eq!((e.msg.as_str(), e.skip), ("a; v", false));
        let e = add_failure(Ok(String::new()), rise()).unwrap_err();
        assert_eq!(e.msg, "v");
        let e = add_failure(Ok(()), rise()).unwrap_err();
        assert_eq!(e.msg, "v");
        let e = add_failure(Err::<(), _>(Fail::skip("a")), rise()).unwrap_err();
        assert_eq!((e.msg.as_str(), e.skip), ("a; v", false));
        let e = add_failure(Ok(()), Err(Fail::skip("s"))).unwrap_err();
        assert_eq!((e.msg.as_str(), e.skip), ("s", false));
    }

    #[test]
    fn keys() {
        assert_eq!(
            "Enter = done, s + Enter = skip this check",
            keys_text(ACTION)
        );
        assert_eq!(
            "y + Enter = yes, n + Enter = no, s + Enter = skip this check",
            keys_text(QUESTION)
        );
    }
}
