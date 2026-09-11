//! Byte-level tests for [`ControlSequenceFilter`]. No real terminal is
//! touched; only the returned bytes are asserted.

use super::*;

fn push_all(chunks: &[&str]) -> String {
    let mut filter = ControlSequenceFilter::new();
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&filter.push(chunk));
    }
    out
}

#[test]
fn plain_and_cjk_text_passes_through() {
    assert_eq!(push_all(&["hello 世界\n"]), "hello 世界\n");
    assert_eq!(strip_ansi("普通中文文本"), "普通中文文本");
}

#[test]
fn csi_sequences_are_removed() {
    assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
    assert_eq!(strip_ansi("a\x1b[Kb\x1b[?25lc"), "abc");
}

#[test]
fn osc52_with_bel_terminator_is_removed() {
    assert_eq!(strip_ansi("\x1b]52;c;cGF5bG9hZA==\x07visible"), "visible");
}

#[test]
fn osc_with_esc_backslash_terminator_is_removed() {
    assert_eq!(strip_ansi("x\x1b]0;title\x1b\\y"), "xy");
    assert_eq!(strip_ansi("x\x1b]8;;https://e.test\x1b\\link"), "xlink");
}

#[test]
fn unfinished_osc_does_not_swallow_the_next_message_after_reset() {
    let mut filter = ControlSequenceFilter::new();
    assert_eq!(filter.push("\x1b]0;unterminated title"), "");
    // Stream boundary: the parser must not keep consuming the next message.
    filter.reset();
    assert_eq!(filter.push("next task body"), "next task body");
}

#[test]
fn csi_split_across_chunks_is_removed() {
    let mut filter = ControlSequenceFilter::new();
    assert_eq!(filter.push("abc\x1b["), "abc");
    assert_eq!(filter.push("31mdef"), "def");
}

#[test]
fn osc_split_across_chunks_is_removed() {
    let mut filter = ControlSequenceFilter::new();
    assert_eq!(filter.push("a\x1b]52;c;"), "a");
    assert_eq!(filter.push("AAAA\x07b"), "b");
    // ESC \ terminator split between the ESC and the backslash.
    assert_eq!(filter.push("c\x1b]0;t\x1b"), "c");
    assert_eq!(filter.push("\\d"), "d");
}

#[test]
fn incomplete_trailing_escape_is_dropped_and_does_not_leak() {
    let mut filter = ControlSequenceFilter::new();
    assert_eq!(filter.push("tail\x1b"), "tail");
    assert_eq!(filter.push(""), "");
    // A late reset still yields nothing for the abandoned sequence.
    filter.reset();
    assert_eq!(filter.push("ok"), "ok");
}

#[test]
fn bel_and_stray_control_chars_are_dropped() {
    assert_eq!(strip_ansi("ding\x07dong\x08"), "dingdong");
    assert_eq!(push_all(&["tab\tkept\n"]), "tab\tkept\n");
}

#[test]
fn c1_forms_are_removed() {
    // U+009B is the C1 CSI; U+009D is the C1 OSC.
    assert_eq!(strip_ansi("\u{009b}31mred"), "red");
    assert_eq!(strip_ansi("\u{009d}0;title\u{0007}after"), "after");
}

#[test]
fn runaway_osc_aborts_after_the_bound() {
    let mut filter = ControlSequenceFilter::new();
    let flood = format!("\x1b]0;{}", "a".repeat(MAX_SEQUENCE_CHARS + 10));
    let _ = filter.push(&flood);
    assert_eq!(filter.push("tail"), "tail");
}
