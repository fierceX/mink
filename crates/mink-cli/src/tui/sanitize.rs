// Control-sequence stripping is shared with the REPL (`ui::sanitize`); the
// TUI keeps its own layout normalization below (tab expansion, CR/LF).
pub(crate) use crate::ui::sanitize::strip_ansi;

pub(crate) fn sanitize_tui_text(input: &str) -> String {
    normalize_control_text(&strip_ansi(input))
}

pub(crate) fn normalize_tui_input(input: &str) -> String {
    normalize_control_text(input)
}

fn normalize_control_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}
