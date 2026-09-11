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

#[test]
fn unterminated_osc_in_error_does_not_hide_next_error() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_error("bad \x1b]0;unterminated");
    display.render_error("second independent error");

    let text = err.text();
    assert!(
        text.contains("second independent error"),
        "each error must be sanitized as a complete message: {text:?}"
    );
}

#[test]
fn unterminated_osc_in_thinking_does_not_hide_final_answer() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_thinking("thinking \x1b]0;unterminated");
    display.render_text("final answer");

    let text = out.text();
    assert!(
        text.contains("final answer"),
        "the thinking → text boundary must reset the parser: {text:?}"
    );
}

#[test]
fn failed_stream_without_stop_resets_parser_for_next_turn() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    // A stream that dies without Stop leaves a dirty parser unless the error
    // path resets it; the next turn's thinking must still render.
    display.render_thinking("\x1b]0;unterminated");
    display.render_error("turn failed");
    display.render_thinking("next turn thinking");

    let text = out.text();
    assert!(
        text.contains("next turn thinking"),
        "a failed stream must not leak parser state into the next turn: {text:?}"
    );
}

#[test]
fn tool_result_boundary_resets_parser_for_following_text() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_tool_result(&crate::ui::PresentedToolResultDisplay {
        base: crate::ui::ToolResultDisplay {
            tool_name: "Read",
            content_preview: "preview \x1b]0;unterminated",
            content: "",
            tool_use_id: Some("t1"),
            exit_code: None,
        },
        status: crate::runtime::ToolStatus::Succeeded,
        result_kind: crate::ui::ToolResultKind::Text,
        presentation: None,
        artifacts: &[],
    });
    display.render_text("body after tool result");

    let text = out.text();
    assert!(
        text.contains("body after tool result"),
        "the tool-result boundary must reset the parser: {text:?}"
    );
}

#[test]
fn sub_agent_output_resets_between_thinking_and_text() {
    let out = SharedBuffer::default();
    let err = SharedBuffer::default();
    let display = display_with(&out, &err, false);

    display.render_sub_agent_output(
        "child",
        "ok",
        "thinking \x1b]0;unterminated",
        "child final answer",
        1,
        2,
    );

    let text = out.text();
    assert!(
        text.contains("child final answer"),
        "the sub-agent body must not be swallowed by thinking's parser: {text:?}"
    );

    // The sub-agent block must also leave the main stream parser clean.
    display.render_text("main body");
    let text = out.text();
    assert!(
        text.contains("main body"),
        "the block boundary must leave the main parser clean: {text:?}"
    );
}
