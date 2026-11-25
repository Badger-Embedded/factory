//! Reads a stage's outcome from an agent's final message, for a run that
//! returns its JSON in that message, bare or in a closing fenced block.

use serde_json::Value;

use crate::agent::RunResult;
use crate::outcome::{Expect, event_from_output, failed_event};

/// The result of a run whose final message is `text`: the message itself as
/// JSON, or its last fenced block.
pub(crate) fn message_result(expect: Expect, text: &str) -> RunResult {
    let event = match serde_json::from_str::<Value>(json_slice(text)) {
        Ok(value) => event_from_output(expect, Some(value)),
        Err(err) => failed_event(
            expect,
            format!("the agent's final message is not valid JSON: {err}"),
        ),
    };
    RunResult {
        event,
        session_id: None,
        cost_usd: None,
    }
}

/// The JSON to parse from the final message: the content of the fenced block
/// that ends the message, whatever text precedes it, or the whole trimmed text
/// when the message does not end with a closing fence. A fence line starts with
/// at least three backticks after optional indentation. A block closes on a
/// bare fence at least as long as its opening fence. Backticks inside a valid
/// JSON string cannot start a raw line.
pub(crate) fn json_slice(text: &str) -> &str {
    let text = text.trim();
    let mut open: Option<(usize, usize)> = None;
    let mut body = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        match open {
            None if ticks >= 3 => {
                open = Some((ticks, offset + line.len()));
                body = None;
            }
            None => body = None,
            Some((len, start)) if ticks >= len && ticks == trimmed.len() => {
                body = Some(text[start..offset].trim());
                open = None;
            }
            Some(_) => {}
        }
        offset += line.len();
    }
    body.filter(|_| open.is_none()).unwrap_or(text)
}
