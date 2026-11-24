use super::*;

#[test]
fn split_at_visual_width_respects_cjk_width() {
    assert_eq!(split_at_visual_width("ab中c", 3), vec!["ab", "中c"]);
    assert_eq!(split_at_visual_width("", 3), vec![""]);
}

#[test]
fn split_at_visual_width_preserves_explicit_newlines() {
    assert_eq!(split_at_visual_width("a\nb", 10), vec!["a", "b"]);
    assert_eq!(split_at_visual_width("a\n", 10), vec!["a", ""]);
    assert_eq!(split_at_visual_width("\n", 10), vec!["", ""]);
    assert_eq!(split_at_visual_width("ab\n中c", 3), vec!["ab", "中c"]);
}

#[test]
fn strip_ansi_removes_basic_sgr_sequences() {
    assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
}

#[test]
fn strip_ansi_removes_common_non_sgr_sequences_without_swallowing_text() {
    assert_eq!(strip_ansi("a\x1b[Kb\x1b[?25lc"), "abc");
    assert_eq!(strip_ansi("x\x1b]0;title\x07y"), "xy");
    assert_eq!(strip_ansi("x\x1b]8;;https://e.test\x1b\\link"), "xlink");
}

#[test]
fn markdown_normalize_cleans_control_sequences_and_line_endings() {
    assert_eq!(
        normalize_markdown_input("a\r\n\t\x1b[31mred\x1b[0m\x07\rb", false),
        "a\n    red\nb"
    );
}

#[test]
fn wrap_lines_word_preserves_span_styles_across_wraps() {
    let red = Style::default().fg(Color::Red);
    let green = Style::default().fg(Color::Green);
    let lines = vec![Line::from(vec![
        Span::styled("abc", red),
        Span::styled("def", green),
    ])];

    let wrapped = wrap_lines_word(&lines, 4);

    assert_eq!(wrapped.len(), 2);
    assert_eq!(wrapped[0].spans[0].style, red);
    assert_eq!(wrapped[0].spans[1].style, green);
    assert_eq!(wrapped[1].spans[0].style, green);
    assert_eq!(line_text(&wrapped[0]), "abcd");
    assert_eq!(line_text(&wrapped[1]), "ef");
}

#[test]
fn markdown_renderer_handles_heading_list_and_inline_code() {
    let mut lines = Vec::new();

    push_msg(&mut lines, "# Title\n- item `code`", TranscriptKind::Text);

    assert_eq!(line_text(&lines[0]), "Title");
    assert_eq!(line_text(&lines[1]), "- item code");
    assert!(
        lines[0].spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    assert_eq!(lines[1].spans[0].style.fg, Some(Color::Yellow));
    assert_eq!(lines[1].spans[3].content.as_ref(), "code");
    assert_eq!(lines[1].spans[3].style.fg, Some(Color::Yellow));
}

#[test]
fn markdown_parser_builds_block_ir_for_core_blocks() {
    let blocks =
        parse_blocks("# Title\n\n> quote\n- item `code`\n```rust\nfn main() {}\n```\nplain");

    assert!(matches!(
        &blocks[0],
        MdBlock::Heading {
            level: 1,
            content
        } if content == &vec![InlineNode::Text("Title".into())]
    ));
    assert!(matches!(blocks[1], MdBlock::Blank));
    assert!(matches!(
        &blocks[2],
        MdBlock::BlockQuote(content)
            if content == &vec![InlineNode::Text("quote".into())]
    ));
    assert!(matches!(
        &blocks[3],
        MdBlock::ListItem { marker, content }
            if marker == "-" && content == &vec![
                InlineNode::Text("item ".into()),
                InlineNode::Code("code".into())
            ]
    ));
    assert!(matches!(
        &blocks[4],
        MdBlock::CodeBlock { lang, lines }
            if lang.as_deref() == Some("rust") && lines == &vec!["fn main() {}".to_string()]
    ));
    assert!(matches!(
        &blocks[5],
        MdBlock::Paragraph(content)
            if content == &vec![InlineNode::Text("plain".into())]
    ));
}

#[test]
fn markdown_parser_keeps_unclosed_code_fence_as_code_block() {
    let blocks = parse_blocks("```text\nopen\nstill open");

    assert_eq!(blocks.len(), 1);
    assert!(matches!(
        &blocks[0],
        MdBlock::CodeBlock { lang, lines }
            if lang.as_deref() == Some("text")
                && lines == &vec!["open".to_string(), "still open".to_string()]
    ));
}

#[test]
fn markdown_renderer_aligns_pipe_tables() {
    let mut lines = Vec::new();

    push_msg(
        &mut lines,
        "| Name | Count | Note |\n| :--- | ---: | :---: |\n| 中 | 2 | ok |\n| long-name | 10 | yes |",
        TranscriptKind::Text,
    );

    assert_eq!(line_text(&lines[0]), "Name      │ Count │ Note");
    assert_eq!(line_text(&lines[1]), "──────────┼───────┼─────");
    assert_eq!(line_text(&lines[2]), "中        │     2 │  ok ");
    assert_eq!(line_text(&lines[3]), "long-name │    10 │ yes ");
    assert!(
        lines[0].spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn markdown_renderer_keeps_escaped_pipe_inside_table_cell() {
    let mut lines = Vec::new();

    push_msg(
        &mut lines,
        "| Pattern | Meaning |\n| --- | --- |\n| `a\\|b` | escaped pipe |",
        TranscriptKind::Text,
    );

    assert_eq!(line_text(&lines[0]), "Pattern │ Meaning     ");
    assert_eq!(line_text(&lines[2]), "`a|b`   │ escaped pipe");
}

#[test]
fn markdown_table_wraps_long_cells_without_omitting_content() {
    let mut lines = Vec::new();

    push_msg_with_width(
        &mut lines,
        "| Key | Description |\n| --- | --- |\n| row | abcdefghijklmnopqrstuvwxyz0123456789 |",
        TranscriptKind::Text,
        24,
        None,
    );

    let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(!rendered.contains('…'));
    assert!(rendered.contains("abcdefghijklmnopqr"));
    assert!(rendered.contains("stuvwxyz0123456789"));
}

#[test]
fn markdown_table_uses_available_width_before_wrapping_cells() {
    let mut lines = Vec::new();

    push_msg_with_width(
        &mut lines,
        "| Key | Description |\n| --- | --- |\n| row | abcdefghijklmnopqrstuvwxyz0123456789 |",
        TranscriptKind::Text,
        80,
        None,
    );

    assert_eq!(lines.len(), 3);
    assert!(line_text(&lines[2]).contains("abcdefghijklmnopqrstuvwxyz0123456789"));
}

#[test]
fn markdown_table_renderer_falls_back_for_invalid_table_rows() {
    let mut lines = Vec::new();
    let table = TableRows {
        header: vec!["A".into(), "B".into()],
        alignments: vec![TableAlign::Left],
        rows: vec![vec!["1".into(), "2".into()], vec!["too-short".into()]],
    };

    render_table(&mut lines, &table, Style::default(), 80);

    assert_eq!(line_text(&lines[0]), "A | B");
    assert_eq!(line_text(&lines[1]), "1 | 2");
    assert_eq!(line_text(&lines[2]), "too-short");
}

#[test]
fn markdown_renderer_styles_strong_emphasis_and_links() {
    let mut lines = Vec::new();

    push_msg(
        &mut lines,
        "Use **bold** and *em* plus [docs](https://example.com)",
        TranscriptKind::Text,
    );

    assert_eq!(
        line_text(&lines[0]),
        "Use bold and em plus docs (https://example.com)"
    );
    assert!(
        lines[0].spans[1]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    assert!(
        lines[0].spans[3]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::ITALIC)
    );
    assert_eq!(lines[0].spans[5].style.fg, Some(Color::Blue));
    assert!(
        lines[0].spans[5]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED)
    );
    assert_eq!(lines[0].spans[6].style.fg, Some(Color::DarkGray));
}

#[test]
fn markdown_renderer_keeps_blockquote_inline_styles() {
    let mut lines = Vec::new();

    push_msg(
        &mut lines,
        "> quoted **bold** and [docs](https://example.com/a_(b))",
        TranscriptKind::Text,
    );

    assert_eq!(
        line_text(&lines[0]),
        "| quoted bold and docs (https://example.com/a_(b))"
    );
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
    assert!(
        lines[0].spans[2]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    assert_eq!(lines[0].spans[4].style.fg, Some(Color::Blue));
    assert!(
        lines[0].spans[4]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED)
    );
}

#[test]
fn colored_tool_diff_is_detected_after_normalization() {
    let mut lines = Vec::new();

    push_msg_with_tool(
        &mut lines,
        "\x1b[31m--- a/file\x1b[0m\n\x1b[32m+++ b/file\x1b[0m\n@@ -1 +1 @@\n-old\n+new",
        TranscriptKind::Tool,
        "Edit",
    );

    assert_eq!(line_text(&lines[0]), "--- a/file");
    assert_eq!(line_text(&lines[1]), "+++ b/file");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Yellow));
    assert_eq!(lines[2].spans[0].style.fg, Some(Color::Cyan));
    assert_eq!(lines[3].spans[0].style.fg, Some(Color::Red));
    assert_eq!(lines[4].spans[0].style.fg, Some(Color::Green));
}

#[test]
fn malformed_table_separator_falls_back_to_plain_lines() {
    let mut lines = Vec::new();

    push_msg(
        &mut lines,
        "| A | B |\n| nope | --- |\nplain",
        TranscriptKind::Text,
    );

    assert_eq!(line_text(&lines[0]), "| A | B |");
    assert_eq!(line_text(&lines[1]), "| nope | --- |");
    assert_eq!(line_text(&lines[2]), "plain");
}

#[test]
fn inline_stream_promotes_complete_markdown_blocks_before_stop() {
    let mut state = TuiState::default();
    state.apply(&TuiSignal::Text(
        "first paragraph\n\nsecond paragraph".into(),
    ));

    state.promote_stable_stream_prefix();

    assert_eq!(state.transcript.lines.len(), 1);
    assert_eq!(state.transcript.lines[0].text, "first paragraph\n\n");
    assert_eq!(state.stream.stream_line, "second paragraph");
    assert!(state.transcript.lines[0].sealed);
}

#[test]
fn info_heartbeat_during_streaming_does_not_split_markdown_fence() {
    // 心跳在未闭合的代码围栏中间到达：文本不得被切段，否则下半段丢失围栏
    // 上下文，围栏内的注释行会被当成 markdown 重新解析。
    let text = r#"**核心思路**：示例说明。

```
data/demo/material/
├── _library/                          # 类型库（不参与查找）
│   ├── types.json                     # 入口键全集：key → type
│   │                                  #    {"a":"A",
│   │                                  #     "b":"B", …}
│   ├── sources/示例素材-0000/          # 原始原件归档（只存一份）
│   └── 校验报告-<日期>.md               # 每类型校验
│
├── <entry>_<branch>/                   # 每个业务入口（自包含）
│   ├── type.json                      # {"type":"A","type_version":"2025-01-01"}
│   ├── framework.json                 # 章节框架 → 结构化章节树
│   ├── shared.md                      # 跨源固定模板块
│   ├── sources/
│   │   ├── <分支ID>/                    # 分支 = 素材版本（01.01/02.02、A/B…）
│   │   │   ├── index.md               # 来源 / 适用说明
│   │   │   ├── 工艺工况.md             # 内容提炼（匹配判断用）
│   │   │   └── 原文.md                 # 抽取正文
│   │   └── …
│   └── output/                        # 占位（可无）
│
└── （不再有旧式手摆目录）
```

## 5. 后续

继续讨论。
"#;
    let split = text.find("01.01/02.02").unwrap();
    let (head, tail) = text.split_at(split);

    let mut state = TuiState::default();
    state.apply(&TuiSignal::Text(head.into()));
    state.apply(&TuiSignal::Info(mink::runtime::llm_wait_heartbeat_message(
        30, 0,
    )));
    // 心跳不打断流：尚未落入 transcript，状态栏标签就位。
    assert!(state.stream.streaming);
    assert_eq!(state.transcript.lines.len(), 0);
    assert_eq!(state.stream.stream_status.as_deref(), Some("·30s"));
    state.apply(&TuiSignal::Text(tail.into()));
    // 新内容到达后等待标签清除。
    assert_eq!(state.stream.stream_status, None);
    state.apply(&TuiSignal::Stop);

    // 最终 transcript：完整文本一条，心跳不落盘（瞬态状态）。
    assert_eq!(state.transcript.lines.len(), 1);
    assert_eq!(state.transcript.lines[0].text, text);
    assert!(!state.stream.streaming);

    // Render the final transcript the same way the TUI does.
    let mut rendered = String::new();
    for item in &state.transcript.lines {
        for line in crate::tui::render::transcript_item_lines(item, 104) {
            rendered.push_str(&line_text(&line));
            rendered.push('\n');
        }
    }

    // 跨心跳边界的那一行必须完整：被切段时两半会渲染成不同行。
    assert!(
        rendered.contains("# 分支 = 素材版本（01.01/02.02、A/B…）"),
        "fence line split at heartbeat; rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("├── _library/                          # 类型库（不参与查找）"),
        "tree line before heartbeat lost; rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("└── 原文.md                 # 抽取正文"),
        "tree line after heartbeat lost; rendered:\n{rendered}"
    );
    // 围栏闭合后的内容按 markdown 渲染（标题不再被代码块吞掉）。
    assert!(
        rendered.contains("5. 后续"),
        "content after fence missing; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("Waiting for model response"),
        "heartbeat should not be persisted into transcript; rendered:\n{rendered}"
    );
}

#[test]
fn markdown_trailing_newline_does_not_create_an_extra_blank_row() {
    let mut lines = Vec::new();

    push_msg(&mut lines, "hello\n", TranscriptKind::Text);

    assert_eq!(lines.len(), 1);
    assert_eq!(line_text(&lines[0]), "hello");
}

#[test]
fn inline_markdown_unmatched_delimiters_stay_linear() {
    let line = "[".repeat(4_000) + &"*".repeat(4_000) + &"`".repeat(4_000);
    let started = std::time::Instant::now();
    let mut lines = Vec::new();
    markdown::push_msg_with_width(&mut lines, &line, TranscriptKind::StreamText, 80, None);
    assert!(!lines.is_empty());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "unmatched delimiters took {:?}",
        started.elapsed()
    );
}

#[test]
fn truncate_display_respects_max_width_smaller_than_ellipsis() {
    assert_eq!(crate::util::truncate_display("hello", 0), "");
    assert_eq!(crate::util::truncate_display("hello", 1), ".");
    assert_eq!(crate::util::truncate_display("hello", 2), "..");
    assert_eq!(crate::util::truncate_display("hello", 3), "...");
    assert_eq!(crate::util::truncate_display("hello", 4), "h...");
}
