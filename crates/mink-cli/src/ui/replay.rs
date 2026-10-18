use serde_json::Value;
use std::io::Write;
use std::path::Path;

use crate::replay::ReplayEventKind;
use crate::session::store::first_line;
use crate::ui::sanitize::{ControlSequenceFilter, strip_ansi};
use crate::util::truncate_display;

/// Replay last N turns from events.jsonl synchronously to stdout/stderr.
pub fn replay_last_turns(events_path: &Path) {
    if !events_path.exists() {
        return;
    }

    let data = match std::fs::read_to_string(events_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    let events: Vec<Value> = data
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    if events.is_empty() {
        return;
    }

    let mut turn_starts: Vec<usize> = Vec::new();
    for (i, evt) in events.iter().enumerate() {
        if crate::replay::classify_event(evt) == ReplayEventKind::UserInput {
            turn_starts.push(i);
        }
    }

    if turn_starts.is_empty() {
        return;
    }

    let keep = turn_starts.len().saturating_sub(10);
    let start_idx = if keep < turn_starts.len() {
        turn_starts[keep]
    } else {
        0
    };
    let had_turns = turn_starts.len().saturating_sub(keep) > 0;

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    render_events(&events[start_idx..], &mut stdout, &mut stderr);
    if had_turns {
        let _ = writeln!(stdout);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Thinking,
    Text,
}

/// Replay-local escape parsers. `thinking`/`text` are continuous streams (the
/// parser keeps state across chunks); every other event is a complete message
/// and starts from a clean parser so an unterminated sequence cannot swallow
/// the next line. stdout and stderr are independent surfaces.
#[derive(Default)]
struct ReplaySanitizer {
    stdout: ControlSequenceFilter,
    stderr: ControlSequenceFilter,
    kind: Option<StreamKind>,
}

impl ReplaySanitizer {
    fn begin_message(&mut self) {
        self.stdout.reset();
        self.kind = None;
    }

    fn stream(&mut self, kind: StreamKind, chunk: &str) -> String {
        if self.kind != Some(kind) {
            self.stdout.reset();
            self.kind = Some(kind);
        }
        self.stdout.push(chunk)
    }

    fn message(&mut self, text: &str) -> String {
        self.begin_message();
        strip_ansi(text)
    }

    fn error(&mut self, text: &str) -> String {
        self.stderr.reset();
        self.stderr.push(text)
    }
}

fn render_events(events: &[Value], stdout: &mut dyn Write, stderr: &mut dyn Write) {
    let mut filter = ReplaySanitizer::default();
    let mut last_char = '\n';
    let mut prev_was_thinking = false;

    for evt in events {
        match crate::replay::classify_event(evt) {
            // Goal lifecycle rows are mirrored by the TUI status marker rather
            // than replayed as transcript lines.
            ReplayEventKind::Ignored
            | ReplayEventKind::PrefixSnapshot
            | ReplayEventKind::GoalLifecycle
            | ReplayEventKind::SubAgent => continue,
            ReplayEventKind::UserInput => {
                flush_newline(stdout, &mut last_char, prev_was_thinking);
                prev_was_thinking = false;
                let content = filter.message(crate::replay::event_content(evt));
                if !content.is_empty() {
                    let preview = truncate_display(first_line(&content), 77);
                    let _ = writeln!(stderr, "\x1b[32m> {preview}\x1b[0m");
                }
                last_char = '\n';
            }
            ReplayEventKind::Thinking => {
                let content =
                    filter.stream(StreamKind::Thinking, crate::replay::event_content(evt));
                let _ = write!(stdout, "\x1b[90m{content}\x1b[0m");
                let _ = stdout.flush();
                if let Some(c) = content.chars().last() {
                    last_char = c;
                }
                prev_was_thinking = true;
            }
            ReplayEventKind::Text => {
                if prev_was_thinking && last_char != '\n' {
                    let _ = writeln!(stdout);
                    last_char = '\n';
                }
                let content = filter.stream(StreamKind::Text, crate::replay::event_content(evt));
                let _ = write!(stdout, "{content}");
                let _ = stdout.flush();
                if let Some(c) = content.chars().last() {
                    last_char = c;
                }
                prev_was_thinking = false;
            }
            ReplayEventKind::ToolCall => {
                flush_newline(stdout, &mut last_char, prev_was_thinking);
                prev_was_thinking = false;
                let name = crate::replay::event_name(evt);
                let summary = filter.message(&crate::replay::build_tool_summary(name, evt));
                let _ = writeln!(stdout, "\x1b[33m[tool] {summary}\x1b[0m");
                last_char = '\n';
            }
            ReplayEventKind::ToolResult => {
                flush_newline(stdout, &mut last_char, prev_was_thinking);
                prev_was_thinking = false;
                let name = crate::replay::event_name(evt);
                let content = filter.message(crate::replay::event_content(evt));
                let preview = if name == "Edit" || name == "Read" || name == "Write" {
                    first_line(&content).to_string()
                } else {
                    truncate_display(&content, 200)
                };
                if !preview.is_empty() {
                    let _ = writeln!(stdout, "{preview}");
                    last_char = '\n';
                }
            }
            ReplayEventKind::Error => {
                flush_newline(stdout, &mut last_char, prev_was_thinking);
                prev_was_thinking = false;
                let msg = filter.error(crate::replay::event_message(evt));
                let _ = writeln!(stderr, "\x1b[31mError: {msg}\x1b[0m");
                last_char = '\n';
            }
            ReplayEventKind::AssistantMessage => {
                flush_newline(stdout, &mut last_char, prev_was_thinking);
                prev_was_thinking = false;
                let text = evt.get("text").and_then(Value::as_str).unwrap_or("");
                let text = filter.message(text);
                if !text.is_empty() {
                    let _ = write!(stdout, "{text}");
                    let _ = stdout.flush();
                }
                if let Some(tool_calls) = evt.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let name = tc.get("name").and_then(Value::as_str).unwrap_or("");
                        let summary = filter.message(&crate::replay::build_tool_summary(name, tc));
                        let _ = writeln!(stdout, "\x1b[33m[tool] {summary}\x1b[0m");
                    }
                }
                last_char = '\n';
            }
            ReplayEventKind::Unknown => {}
        }
    }

    flush_newline(stdout, &mut last_char, prev_was_thinking);
}

fn flush_newline(stdout: &mut dyn Write, last_char: &mut char, prev_was_thinking: bool) {
    if prev_was_thinking && *last_char != '\n' {
        let _ = writeln!(stdout);
        *last_char = '\n';
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(events: &[Value]) -> (String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render_events(events, &mut stdout, &mut stderr);
        (
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn replay_strips_control_sequences_from_every_payload() {
        let events = vec![
            json!({"type":"user_input","content":"hello\x1b]0;pwned\x07\nsecond"}),
            json!({"type":"thinking","content":"think \x1b]52;c;cGF5bG9hZA==\x07"}),
            json!({"type":"text","content":"a\x1b[31mred"}),
            json!({"type":"tool_call","name":"Read","id":"1","input":{"path":"a\x1b]0;x\x07"}}),
            json!({"type":"tool_result","name":"Read","content":"ok\x1b[2J"}),
            json!({"type":"assistant_message","text":"done\x1b]0;pwned\x07"}),
            json!({"type":"error","message":"bad\x1b]0;pwned\x07"}),
        ];
        let (out, err) = render(&events);
        let plain = strip_ansi(&out);
        assert!(!out.contains("pwned"), "{out:?}");
        assert!(!out.contains("\x1b]52"), "{out:?}");
        assert!(!out.contains("\x1b[2J"), "{out:?}");
        assert!(!err.contains("pwned"), "{err:?}");
        assert!(plain.contains("think"), "{plain:?}");
        assert!(plain.contains("ared"), "{plain:?}");
        assert!(plain.contains("done"), "{plain:?}");
        assert!(err.contains("> hello"), "{err:?}");
        assert!(!err.contains("second"), "user preview must stay one line");
        assert!(err.contains("Error: bad"), "{err:?}");
    }

    #[test]
    fn replay_keeps_parser_state_across_streamed_chunks() {
        let events = vec![
            json!({"type":"thinking","content":"a\x1b]5"}),
            json!({"type":"thinking","content":"2;c;x\x07b"}),
            json!({"type":"text","content":"c\x1b["}),
            json!({"type":"text","content":"31md"}),
        ];
        let (out, _) = render(&events);
        let plain = strip_ansi(&out);
        assert!(plain.contains("ab"), "{plain:?}");
        assert!(plain.contains("cd"), "{plain:?}");
    }
}
