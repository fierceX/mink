//! Shared sanitization for untrusted text before it reaches a terminal.
//!
//! Control-sequence stripping lives here so the REPL and TUI share one
//! escape parser; each renderer keeps its own layout policy (newline/tab
//! normalization). [`ControlSequenceFilter`] is stateful so escape sequences
//! split across streamed chunks are still removed instead of leaking a
//! partial sequence to the terminal. The owner must call
//! [`ControlSequenceFilter::reset`] at stream/message boundaries so an
//! unterminated sequence cannot swallow the next message's body.

/// Upper bound for a single pending escape sequence; exceeding it aborts the
/// sequence instead of dropping an unbounded amount of following text.
const MAX_SEQUENCE_CHARS: usize = 4096;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum FilterState {
    #[default]
    Normal,
    /// Saw `ESC`, waiting for the sequence kind.
    Escape,
    /// Inside `CSI` (`ESC [` or C1 `U+009B`), ends at a final byte.
    Csi,
    /// Inside `OSC` (`ESC ]` or C1 `U+009D`), ends at `BEL` or `ESC \`.
    Osc,
    /// Inside `OSC`, saw `ESC` (maybe the start of `ST`).
    OscEscape,
    /// Inside a two-byte charset designation (`ESC ( ) * + - . / X`).
    Charset,
}

/// Stateful escape-sequence stripper for streamed untrusted text.
///
/// Normal text and `\n`/`\r`/`\t` pass through; escape sequences (CSI, OSC
/// with `BEL` or `ESC \` terminator, two-byte designations, C1 forms) and
/// stray control characters are dropped. Sequences may span chunk
/// boundaries; incomplete sequences at [`Self::reset`] are discarded.
#[derive(Default)]
pub(crate) struct ControlSequenceFilter {
    state: FilterState,
    consumed: usize,
}

impl ControlSequenceFilter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop any partially consumed sequence. Call at stream/message
    /// boundaries so an unterminated sequence cannot hide later text.
    pub(crate) fn reset(&mut self) {
        self.state = FilterState::Normal;
        self.consumed = 0;
    }

    /// Feed one chunk and return the text that is safe to emit immediately.
    pub(crate) fn push(&mut self, chunk: &str) -> String {
        let mut out = String::with_capacity(chunk.len());
        for ch in chunk.chars() {
            match self.state {
                FilterState::Normal => match ch {
                    '\x1b' => self.begin_sequence(FilterState::Escape),
                    '\u{009b}' => self.begin_sequence(FilterState::Csi),
                    '\u{009d}' => self.begin_sequence(FilterState::Osc),
                    '\n' | '\r' | '\t' => out.push(ch),
                    ch if ch.is_control() => {}
                    ch => out.push(ch),
                },
                FilterState::Escape => match ch {
                    '[' => self.state = FilterState::Csi,
                    ']' => self.state = FilterState::Osc,
                    '(' | ')' | '*' | '+' | '-' | '.' | '/' => self.state = FilterState::Charset,
                    '\x1b' => self.consumed = 0,
                    _ => self.state = FilterState::Normal,
                },
                FilterState::Charset => self.state = FilterState::Normal,
                FilterState::Csi => {
                    self.consumed += 1;
                    if self.sequence_overflowed() || ('\u{40}'..='\u{7e}').contains(&ch) {
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::Osc => {
                    self.consumed += 1;
                    if self.sequence_overflowed() || ch == '\x07' {
                        self.state = FilterState::Normal;
                    } else if ch == '\x1b' {
                        self.state = FilterState::OscEscape;
                    }
                }
                FilterState::OscEscape => match ch {
                    '\\' => self.state = FilterState::Normal,
                    '\x1b' => self.consumed += 1,
                    _ => {
                        self.consumed += 1;
                        self.state = FilterState::Osc;
                    }
                },
            }
        }
        out
    }

    fn begin_sequence(&mut self, state: FilterState) {
        self.state = state;
        self.consumed = 0;
    }

    fn sequence_overflowed(&self) -> bool {
        self.consumed > MAX_SEQUENCE_CHARS
    }
}

/// Stateless one-shot variant for a complete message (non-streamed paths).
pub(crate) fn strip_ansi(input: &str) -> String {
    let mut filter = ControlSequenceFilter::new();
    filter.push(input)
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
