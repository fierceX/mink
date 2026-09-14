use super::{Display, StatsSnapshot};
use crate::ui::sanitize::ControlSequenceFilter;
use crate::util::fmt_k;
use std::io::{self, Write};
use std::sync::Mutex;

pub struct TerminalDisplay {
    interactive: bool,
    stream_json: bool,
    stdout: Mutex<Box<dyn Write + Send>>,
    stderr: Mutex<Box<dyn Write + Send>>,
    state: Mutex<DisplayState>,
    /// Escape parsers for untrusted payloads; stdout/stderr are independent
    /// streams and must not share parser state.
    stdout_filter: Mutex<ControlSequenceFilter>,
    stderr_filter: Mutex<ControlSequenceFilter>,
}

#[derive(Default)]
struct DisplayState {
    last_char: String,
    prev_was_thinking: bool,
    /// Which continuous untrusted stream the stdout escape parser is in.
    /// Chunked text/thinking keep parser state; any message/kind boundary
    /// resets it so an unterminated sequence cannot swallow later content.
    stream_kind: Option<StreamKind>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Thinking,
    Text,
}

impl TerminalDisplay {
    pub fn new(interactive: bool, stream_json: bool) -> Self {
        Self {
            interactive,
            stream_json,
            stdout: Mutex::new(Box::new(io::stdout())),
            stderr: Mutex::new(Box::new(io::stderr())),
            state: Mutex::new(DisplayState::default()),
            stdout_filter: Mutex::new(ControlSequenceFilter::new()),
            stderr_filter: Mutex::new(ControlSequenceFilter::new()),
        }
    }

    /// Test seam: inspect written bytes without touching a real terminal.
    #[cfg(test)]
    pub(crate) fn with_writers(
        interactive: bool,
        stream_json: bool,
        stdout: Box<dyn Write + Send>,
        stderr: Box<dyn Write + Send>,
    ) -> Self {
        Self {
            interactive,
            stream_json,
            stdout: Mutex::new(stdout),
            stderr: Mutex::new(stderr),
            state: Mutex::new(DisplayState::default()),
            stdout_filter: Mutex::new(ControlSequenceFilter::new()),
            stderr_filter: Mutex::new(ControlSequenceFilter::new()),
        }
    }

    fn lock_stdout(&self) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
        self.stdout.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_stderr(&self) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
        self.stderr.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, DisplayState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write trusted renderer text (colors/title codes added here) after
    /// normalization; suppressed in stream-json mode.
    fn write_normalized(&self, s: &str) {
        if self.stream_json {
            return;
        }
        let normalized = normalize_display_text(s, self.interactive);
        let mut stdout = self.lock_stdout();
        let _ = write!(stdout, "{normalized}");
        let _ = stdout.flush();
    }

    /// Strip terminal control sequences from an untrusted payload before it
    /// is wrapped in trusted renderer codes. Returns the emitted text so
    /// callers can keep layout state consistent.
    fn write_untrusted_out(&self, s: &str) -> String {
        let filtered = self.filter_stdout(s);
        self.write_normalized(&filtered);
        filtered
    }

    fn filter_stdout(&self, s: &str) -> String {
        if self.stream_json {
            return String::new();
        }
        let mut filter = self.stdout_filter.lock().unwrap_or_else(|e| e.into_inner());
        filter.push(s)
    }

    fn filter_stderr(&self, s: &str) -> String {
        let mut filter = self.stderr_filter.lock().unwrap_or_else(|e| e.into_inner());
        filter.push(s)
    }

    /// Mark a message boundary: drop any partially consumed escape sequence
    /// so an unterminated one cannot swallow the next message's body.
    fn reset_stdout_filter(&self) {
        self.stdout_filter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset();
    }

    fn reset_stderr_filter(&self) {
        self.stderr_filter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset();
    }

    /// Message boundary: reset the stdout parser and leave streamed-kind
    /// tracking so the next thinking/text block starts clean.
    fn begin_stdout_message(&self) {
        self.reset_stdout_filter();
        self.lock_state().stream_kind = None;
    }

    /// Continuous-stream boundary: parser state is kept inside one
    /// thinking/text block and reset when the block kind changes.
    fn ensure_stream_kind(&self, kind: StreamKind) {
        let changed = {
            let mut state = self.lock_state();
            let changed = state.stream_kind != Some(kind);
            state.stream_kind = Some(kind);
            changed
        };
        if changed {
            self.reset_stdout_filter();
        }
    }

    fn write_err(&self, s: &str) {
        let mut stderr = self.lock_stderr();
        let _ = write!(stderr, "{s}");
        let _ = stderr.flush();
    }

    fn update_last_char(&self, text: &str) {
        let mut state = self.lock_state();
        if text.ends_with('\n') {
            state.last_char = "\n".into();
        } else if let Some(c) = text.chars().last() {
            state.last_char = c.to_string();
        }
    }
}

impl Display for TerminalDisplay {
    fn render_thinking(&self, content: &str) {
        self.ensure_stream_kind(StreamKind::Thinking);
        let filtered = self.filter_stdout(content);
        self.write_normalized(&format!("\x1b[90m{filtered}\x1b[0m"));
        self.update_last_char(&filtered);
        self.lock_state().prev_was_thinking = true;
    }

    fn render_text(&self, content: &str) {
        self.ensure_stream_kind(StreamKind::Text);
        {
            let state = self.lock_state();
            if state.prev_was_thinking && state.last_char != "\n" {
                drop(state);
                self.write_normalized("\n");
                self.lock_state().last_char = "\n".into();
            }
        }
        let filtered = self.write_untrusted_out(content);
        self.update_last_char(&filtered);
        self.lock_state().prev_was_thinking = false;
    }

    fn render_tool_call(&self, call: &crate::ui::ToolCallDisplay<'_>) {
        self.begin_stdout_message();
        {
            let state = self.lock_state();
            if state.last_char != "\n" {
                drop(state);
                self.write_normalized("\n");
                self.lock_state().last_char = "\n".into();
            }
        }
        let summary = self.filter_stdout(call.summary);
        self.write_normalized(&format!("\x1b[33m[tool] {summary}\x1b[0m\n"));
        self.lock_state().last_char = "\n".into();
        self.lock_state().prev_was_thinking = false;
    }

    fn render_tool_result(&self, result: &crate::ui::PresentedToolResultDisplay<'_>) {
        self.begin_stdout_message();
        {
            let state = self.lock_state();
            if state.prev_was_thinking && state.last_char != "\n" {
                drop(state);
                self.write_normalized("\n");
                self.lock_state().last_char = "\n".into();
            }
        }
        self.lock_state().prev_was_thinking = false;
        if !result.base.content_preview.is_empty() {
            let filtered = self.write_untrusted_out(result.base.content_preview);
            self.update_last_char(&filtered);
        }
    }

    fn render_stop(&self, _reason: &str) {
        self.begin_stdout_message();
        let state = self.lock_state();
        if state.last_char != "\n" {
            drop(state);
            self.write_normalized("\n");
            self.lock_state().last_char = "\n".into();
        }
    }

    fn render_error(&self, message: &str) {
        // Errors are complete messages and mark a stream interruption: reset
        // both parsers so one unterminated sequence cannot hide the next
        // independent error or the following turn's output.
        self.reset_stderr_filter();
        self.begin_stdout_message();
        {
            let state = self.lock_state();
            if state.last_char != "\n" {
                drop(state);
                self.write_err("\n");
            }
        }
        let filtered = self.filter_stderr(message);
        self.write_err(&format!("\x1b[31mError: {filtered}\x1b[0m\n"));
    }

    fn render_retry(&self) {
        self.begin_stdout_message();
        self.write_err("RETRY\n");
    }

    fn render_signal(&self, _signal_kind: &str, _severity: f64, _message: &str) {}

    fn render_info(&self, msg: &str) {
        self.write_err(&format!("\x1b[36m{msg}\x1b[0m\n"));
    }

    fn render_title_update(&self, model: &str, stats: &StatsSnapshot) {
        let total_in = stats.total_input_tokens + stats.total_cache_read_tokens;
        let belief_str = if stats.belief > 0.0 {
            format!(" B:{:.2}", stats.belief)
        } else {
            String::new()
        };
        let msg = format!(
            "\x1b]0;{}{belief_str} T:{} R:{} I:{}({}) O:{} C:{}({})\x07",
            model,
            StatsSnapshot::fmt_num(stats.current_turn_count),
            StatsSnapshot::fmt_num(stats.agent_request_count),
            fmt_k(total_in),
            stats.cache_pct(),
            fmt_k(stats.total_output_tokens),
            fmt_k(stats.current_context_tokens),
            stats.ctx_pct(),
        );
        self.write_err(&msg);
    }

    fn render_sub_agent_status(
        &self,
        session_id: &str,
        status: &str,
        in_tokens: u64,
        out_tokens: u64,
    ) {
        if status == "ok" || status == "launched" || status == "running" {
            self.write_err(&format!(
                "\x1b[35m[sub-agent {}] {} (in={}, out={})\x1b[0m\n",
                session_id, status, in_tokens, out_tokens
            ));
        } else {
            self.write_err(&format!(
                "\x1b[31m[sub-agent {}] failed\x1b[0m\n",
                session_id
            ));
        }
    }

    fn render_sub_agent_output(
        &self,
        session_id: &str,
        status: &str,
        thinking: &str,
        text: &str,
        in_tokens: u64,
        out_tokens: u64,
    ) {
        // Independent message block: do not share parser state with the main
        // stream in either direction.
        self.begin_stdout_message();
        self.write_normalized(&format!(
            "[sub-agent {}] {} (in={}, out={})\n",
            session_id, status, in_tokens, out_tokens,
        ));
        if !thinking.is_empty() {
            // Each section is a complete message: reset the parser so an
            // unterminated sequence in thinking cannot swallow the body.
            self.begin_stdout_message();
            self.write_normalized("── Thinking ──\n");
            let filtered = self.write_untrusted_out(thinking);
            if !filtered.ends_with('\n') {
                self.write_normalized("\n");
            }
        }
        if !text.is_empty() {
            self.begin_stdout_message();
            self.write_normalized("── Text ──\n");
            let filtered = self.write_untrusted_out(text);
            if !filtered.ends_with('\n') {
                self.write_normalized("\n");
            }
        }
        self.begin_stdout_message();
    }

    fn render_prompt(&self) {
        self.begin_stdout_message();
        self.write_err("\x1b[32m> \x1b[0m");
    }

    fn render_clear_line(&self) {
        self.begin_stdout_message();
        self.write_err("\r\x1b[2K");
    }
}

fn normalize_display_text(s: &str, interactive: bool) -> String {
    let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
    if interactive {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
