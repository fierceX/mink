//! Byte-level tests for `TerminalDisplay`'s untrusted-payload sanitization.
//! Only the written bytes are asserted; no real terminal is involved.

use super::*;
use std::sync::Arc;

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes).expect("display output must be valid UTF-8")
    }
}

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn display_with(
    stdout: &SharedBuffer,
    stderr: &SharedBuffer,
    interactive: bool,
) -> TerminalDisplay {
    TerminalDisplay::with_writers(
        interactive,
        false,
        Box::new(stdout.clone()),
        Box::new(stderr.clone()),
    )
}

#[test]
fn render_text_strips_control_sequences_across_chunks() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_text("a\x1b]52;c;cGF5bG9hZA==\x07b");
    display.render_text("\x1b[");
    display.render_text("31mred");

    assert_eq!(out.text(), "abred");
    assert_eq!(err.text(), "");
}

#[test]
fn render_error_strips_payload_but_keeps_trusted_codes() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_error("bad \x1b]0;pwned\x07message");

    let text = err.text();
    assert!(text.contains("Error: bad message"), "{text:?}");
    assert!(!text.contains("pwned"), "{text:?}");
    assert!(
        text.contains("\x1b[31m"),
        "trusted color code missing: {text:?}"
    );
}

#[test]
fn render_thinking_keeps_trusted_wrapper_only() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_thinking("think \x1b[31mred");

    assert_eq!(out.text(), "\x1b[90mthink red\x1b[0m");
}

#[test]
fn unfinished_osc_does_not_swallow_the_next_message() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_text("\x1b]0;unterminated title");
    assert_eq!(out.text(), "", "the payload must not be echoed");

    // A message boundary resets the parser: the next message still renders.
    display.render_tool_call(&crate::ui::ToolCallDisplay {
        tool_use_id: "t1",
        tool_name: "Read",
        summary: "file.rs",
        input: None,
    });
    display.render_text("next task body");

    let text = out.text();
    assert!(text.contains("[tool] file.rs"), "{text:?}");
    assert!(text.contains("next task body"), "{text:?}");
}

#[test]
fn interactive_crlf_layout_applies_after_filtering() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, true);

    display.render_text("line1\nline2");

    assert_eq!(out.text(), "line1\r\nline2");
}

#[test]
fn stream_json_mode_writes_nothing() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display =
        TerminalDisplay::with_writers(false, true, Box::new(out.clone()), Box::new(err.clone()));

    display.render_text("text \x1b[31mred");
    display.render_tool_call(&crate::ui::ToolCallDisplay {
        tool_use_id: "t1",
        tool_name: "Read",
        summary: "file.rs",
        input: None,
    });

    assert_eq!(out.text(), "");
}
