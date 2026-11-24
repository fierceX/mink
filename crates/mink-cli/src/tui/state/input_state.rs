use super::*;

/// One clipboard image staged for the next user message. The bytes live in
/// `<session_dir>/attachments/<sha256>.png`; the message only carries the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingImage {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

impl PendingImage {
    /// Stable marker appended to the submitted user text. The model is asked
    /// to `Read` the absolute path; the capture itself still runs through the
    /// v7 image pipeline.
    pub(crate) fn marker(&self) -> String {
        format!(
            "[Attached image: \"{}\" - Read it to view.]",
            self.path.display()
        )
    }

    pub(crate) fn chip(&self, index: usize) -> String {
        format!(
            "[image #{index} {}x{} {}]",
            self.width,
            self.height,
            format_bytes(self.bytes)
        )
    }
}

pub(crate) fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        scaled(value / MB, "MB")
    } else if value >= KB {
        scaled(value / KB, "KB")
    } else {
        format!("{bytes}B")
    }
}

/// At most one decimal: `220KB` instead of `220.0KB`, `1.5KB` keeps its digit.
fn scaled(value: f64, unit: &str) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded - rounded.trunc()).abs() < f64::EPSILON {
        format!("{rounded:.0}{unit}")
    } else {
        format!("{rounded:.1}{unit}")
    }
}

/// Submitted text for the runtime: the typed text plus one marker per image.
pub(crate) fn submitted_user_input(typed: &str, images: &[PendingImage]) -> String {
    if images.is_empty() {
        return typed.to_string();
    }
    let markers = images
        .iter()
        .map(PendingImage::marker)
        .collect::<Vec<_>>()
        .join("\n");
    if typed.is_empty() {
        markers
    } else {
        format!("{typed}\n\n{markers}")
    }
}

/// Transcript echo for one user message: images collapse to `[image #N]`.
pub(crate) fn display_user_input(typed: &str, images: &[PendingImage]) -> String {
    let prefix = (1..=images.len())
        .map(|index| format!("[image #{index}]"))
        .collect::<Vec<_>>()
        .join(" ");
    match (typed.is_empty(), prefix.is_empty()) {
        (true, _) => prefix,
        (false, true) => typed.to_string(),
        (false, false) => format!("{prefix} {typed}"),
    }
}

/// Compact paste markers in replayed user input back to `[image]` so a resumed
/// session does not echo absolute attachment paths.
pub(crate) fn compact_user_input_for_display(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            if line.starts_with("[Attached image: \"") && line.ends_with("\" - Read it to view.]") {
                "[image]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Default)]
pub(crate) struct InputState {
    pub revision: u64,
    pub layout: Option<std::sync::Arc<crate::tui::editor::InputLayout>>,
    pub preferred_column: Option<usize>,
    pub undo: Vec<crate::tui::editor::DraftEdit>,
    pub redo: Vec<crate::tui::editor::DraftEdit>,
    pub buf: String,
    pub cursor: usize,
    pub scroll_row: usize,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub draft_before_history: Option<String>,
    /// Clipboard images queued for the next submitted message.
    pub pending_images: Vec<PendingImage>,
}

impl InputState {
    pub(crate) fn clamped_cursor(&self) -> usize {
        if let Some(layout) = &self.layout
            && layout.revision == self.revision
            && layout.text == self.buf
        {
            layout.cursor(self.cursor.min(self.buf.len())).byte
        } else {
            crate::tui::editor::clamp(&self.buf, self.cursor)
        }
    }

    pub(crate) fn draft_snapshot(&self) -> Self {
        Self {
            revision: self.revision,
            buf: self.buf.clone(),
            cursor: self.cursor,
            pending_images: self.pending_images.clone(),
            ..Default::default()
        }
    }
    pub(crate) fn layout(
        &mut self,
        width: usize,
    ) -> std::sync::Arc<crate::tui::editor::InputLayout> {
        let valid = self.layout.as_ref().is_some_and(|layout| {
            layout.revision == self.revision && layout.width == width && layout.text == self.buf
        });
        if !valid {
            self.layout = Some(std::sync::Arc::new(crate::tui::editor::InputLayout::new(
                &self.buf,
                self.revision,
                width,
            )));
        }
        self.layout.as_ref().expect("input layout").clone()
    }
    pub(crate) fn undo_edit(&mut self, redo: bool) {
        let edit = if redo {
            self.redo.pop()
        } else {
            self.undo.pop()
        };
        if let Some(edit) = edit {
            let current = crate::tui::editor::DraftEdit {
                text: std::mem::replace(&mut self.buf, edit.text),
                cursor: self.cursor,
            };
            if redo {
                self.undo.push(current);
            } else {
                self.redo.push(current);
            }
            self.cursor = edit.cursor;
            self.preferred_column = None;
        }
    }
    pub(crate) fn clamp_cursor(&mut self) {
        self.cursor = self.clamped_cursor();
    }

    pub(crate) fn push_history(&mut self, entry: String) {
        const MAX_INPUT_HISTORY: usize = 500;
        self.history.push(entry);
        if self.history.len() > MAX_INPUT_HISTORY {
            let excess = self.history.len() - MAX_INPUT_HISTORY;
            self.history.drain(..excess);
        }
    }
}

pub(crate) fn clamp_char_boundary(s: &str, pos: usize) -> usize {
    let mut pos = pos.min(s.len());
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}
