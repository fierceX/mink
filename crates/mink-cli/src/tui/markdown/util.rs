use ratatui::text::{Line, Span};

pub(crate) fn wrap_lines_word(lines: &[Line<'static>], max_w: u16) -> Vec<Line<'static>> {
    let mw = max_w.max(1) as usize;
    let mut out = Vec::new();
    for line in lines {
        if line.spans.is_empty() || line.spans.iter().all(|span| span.content.is_empty()) {
            out.push(Line::from(""));
            continue;
        }

        let mut cur: Vec<Span<'static>> = Vec::new();
        let mut cur_w = 0usize;
        for span in &line.spans {
            let style = span.style;
            let mut buf = String::new();
            for ch in span.content.chars() {
                if ch == '\n' {
                    if !buf.is_empty() {
                        cur.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    out.push(Line::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                    continue;
                }

                let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + ch_w > mw && (!cur.is_empty() || !buf.is_empty()) {
                    if !buf.is_empty() {
                        cur.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    out.push(Line::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                    if ch == ' ' {
                        continue;
                    }
                }
                buf.push(ch);
                cur_w += ch_w;
            }
            if !buf.is_empty() {
                cur.push(Span::styled(buf, style));
            }
        }
        if !cur.is_empty() {
            out.push(Line::from(cur));
        }
    }
    out
}

pub(crate) use crate::util::truncate_visual;

#[cfg(test)]
mod tests {
    use super::wrap_lines_word;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    #[test]
    fn wrap_lines_word_preserves_empty_and_styled_blank_lines() {
        let lines = vec![
            Line::from("first"),
            Line::from(""),
            Line::from(Span::styled("", Style::default())),
            Line::from("second"),
        ];
        let wrapped = wrap_lines_word(&lines, 40);
        assert_eq!(wrapped.len(), 4);
        assert_eq!(wrapped[0].to_string(), "first");
        assert_eq!(wrapped[1].to_string(), "");
        assert_eq!(wrapped[2].to_string(), "");
        assert_eq!(wrapped[3].to_string(), "second");
    }
}
