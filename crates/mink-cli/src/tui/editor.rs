use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone)]
pub(crate) struct DraftEdit {
    pub text: String,
    pub cursor: usize,
}
#[derive(Clone, Copy)]
pub(crate) struct CursorPosition {
    pub byte: usize,
    pub row: usize,
    pub col: usize,
}
#[derive(Clone)]
pub(crate) struct InputLayout {
    pub text: String,
    pub revision: u64,
    pub width: usize,
    pub lines: Vec<String>,
    pub positions: Vec<CursorPosition>,
}
impl InputLayout {
    pub fn new(text: &str, revision: u64, width: usize) -> Self {
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut positions = Vec::new();
        let mut line = String::new();
        let mut row = 0;
        let mut col = 0;
        for (byte, grapheme) in text.grapheme_indices(true) {
            let cells = grapheme.width();
            if grapheme != "\n" && col + cells > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                row += 1;
                col = 0;
            }
            positions.push(CursorPosition { byte, row, col });
            if grapheme == "\n" {
                lines.push(std::mem::take(&mut line));
                row += 1;
                col = 0;
            } else {
                line.push_str(grapheme);
                col += cells;
            }
        }
        if col >= width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            row += 1;
            col = 0;
        }
        positions.push(CursorPosition {
            byte: text.len(),
            row,
            col,
        });
        lines.push(line);
        Self {
            text: text.into(),
            revision,
            width,
            lines,
            positions,
        }
    }
    pub fn cursor(&self, byte: usize) -> CursorPosition {
        self.positions[self
            .positions
            .partition_point(|pos| pos.byte <= byte)
            .saturating_sub(1)]
    }
    pub fn vertical(&self, byte: usize, delta: isize, goal: usize) -> usize {
        let row = self
            .cursor(byte)
            .row
            .saturating_add_signed(delta)
            .min(self.lines.len().saturating_sub(1));
        self.positions
            .iter()
            .filter(|pos| pos.row == row)
            .min_by_key(|pos| pos.col.abs_diff(goal))
            .map_or(byte, |pos| pos.byte)
    }
}
pub(crate) fn previous(s: &str, byte: usize) -> usize {
    s[..byte]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(offset, _)| offset)
}
pub(crate) fn next(s: &str, byte: usize) -> usize {
    byte + s[byte..].graphemes(true).next().map_or(0, str::len)
}
pub(crate) fn clamp(s: &str, byte: usize) -> usize {
    if byte >= s.len() {
        return s.len();
    }
    s.grapheme_indices(true)
        .map(|(offset, _)| offset)
        .take_while(|offset| *offset <= byte)
        .last()
        .unwrap_or(0)
}
