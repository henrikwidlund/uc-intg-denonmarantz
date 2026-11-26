// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Questions to the owner on the terminal.

use crate::{Ctx, Fail};

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

#[cfg(test)]
mod tests {
    use super::*;

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
