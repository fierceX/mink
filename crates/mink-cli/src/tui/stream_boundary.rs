//! Incremental complete-line scanner. Ambiguous containers remain mutable.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fence {
    pub marker: u8,
    pub len: usize,
}
impl Fence {
    pub fn opening(line: &str) -> Option<(Self, &str)> {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return None;
        }
        let marker = *trimmed.as_bytes().first()?;
        if !matches!(marker, b'`' | b'~') {
            return None;
        }
        let len = trimmed.bytes().take_while(|byte| *byte == marker).count();
        if len < 3 {
            return None;
        }
        let info = trimmed[len..].trim();
        if marker == b'`' && info.contains('`') {
            return None;
        }
        Some((Self { marker, len }, info))
    }
    pub fn closes(self, line: &str) -> bool {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return false;
        }
        let len = trimmed
            .bytes()
            .take_while(|byte| *byte == self.marker)
            .count();
        len >= self.len && trimmed[len..].trim().is_empty()
    }
}
#[derive(Clone, Default)]
pub(crate) struct StreamBoundary {
    scanned: usize,
    searched: usize,
    fence: Option<Fence>,
    container: bool,
    blank: Option<usize>,
}
impl StreamBoundary {
    pub fn scan(&mut self, text: &str) -> usize {
        let mut stable = 0;
        for (relative, _) in text[self.searched..].match_indices('\n') {
            let end = self.searched + relative + 1;
            let raw = &text[self.scanned..end - 1];
            let trimmed = raw.trim();
            if let Some(fence) = self.fence {
                if fence.closes(raw) {
                    self.fence = None;
                    if !self.container {
                        stable = end;
                    }
                }
            } else if let Some((fence, _)) = Fence::opening(raw) {
                self.fence = Some(fence);
            } else if trimmed.is_empty() {
                self.blank = Some(end);
                if !self.container {
                    stable = end;
                }
            } else {
                let list = ["- ", "* ", "+ "]
                    .iter()
                    .any(|prefix| trimmed.starts_with(prefix))
                    || trimmed.split_once(". ").is_some_and(|(number, _)| {
                        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                    });
                let container = list
                    || trimmed.starts_with('>')
                    || trimmed.contains('|')
                    || raw.starts_with("    ")
                    || raw.starts_with('\t');
                if self.container
                    && !container
                    && let Some(blank) = self.blank.take()
                {
                    stable = blank;
                    self.container = false;
                }
                self.container |= container;
                self.blank = None;
            }
            self.scanned = end;
        }
        self.searched = text.len();
        if stable > 0 {
            self.scanned -= stable;
            self.searched -= stable;
            self.blank = self.blank.map(|offset| offset.saturating_sub(stable));
        }
        stable
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fences_require_matching_marker_length_and_complete_closing_line() {
        for text in [
            "````rust\nx\n```\n\n",
            "```rust\nx\n~~~\n\n",
            "```rust\nx\n```suffix\n\n",
            "```rust\nx\n```",
        ] {
            assert_eq!(StreamBoundary::default().scan(text), 0, "{text}");
        }
        let mut scanner = StreamBoundary::default();
        assert_eq!(scanner.scan("````rust\nx\n````"), 0);
        assert_eq!(
            scanner.scan("````rust\nx\n````\n"),
            "````rust\nx\n````\n".len()
        );
    }
    #[test]
    fn containers_stay_mutable_until_a_complete_successor() {
        for container in [
            "- first\n\n",
            "> quote\n\n",
            "| a | b |\n|---|---|\n| 1 | 2 |\n\n",
        ] {
            let mut scanner = StreamBoundary::default();
            assert_eq!(scanner.scan(container), 0);
            let text = format!("{container}paragraph\n");
            assert_eq!(scanner.scan(&text), container.len());
        }
    }
}
