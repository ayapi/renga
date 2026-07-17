use super::*;

/// What the current text selection is anchored to.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionTarget {
    Pane(usize),
    Preview,
}

/// Text selection state. Works for both terminal panes and the file
/// preview panel — `target` tells rendering and extraction which
/// source to read.
///
/// Coordinate semantics differ by target:
/// - **Pane**: start/end rows+cols are screen-relative to
///   `content_rect` (the inner area of the pane border).
/// - **Preview**: rows are **absolute line indices** into
///   `preview.lines`; cols are **char offsets** within the line.
///   This lets the selection survive vertical and horizontal
///   scrolling — overlay rendering subtracts the current scroll
///   to turn source coords back into screen coords.
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub target: SelectionTarget,
    pub start_row: u32,
    pub start_col: u32,
    pub end_row: u32,
    pub end_col: u32,
    /// Content area used for coordinate mapping — the inside of the
    /// pane border, or (for previews) the area excluding the line
    /// number gutter.
    pub content_rect: Rect,
}

/// Keyboard copy-mode state (Windows Terminal "mark mode" style).
/// While `App::copy_mode` is `Some`, `handle_key_event` routes every
/// key press into `handle_copy_mode_key` so nothing leaks to the PTY.
///
/// Cursor coordinates are screen-relative to the pane's content rect
/// — the same coordinate space as [`TextSelection`] with
/// [`SelectionTarget::Pane`], so the existing selection rendering and
/// text extraction work unchanged.
#[derive(Debug, Clone)]
pub struct CopyModeState {
    /// Pane the mode is anchored to (the pane focused at entry).
    pub pane_id: usize,
    pub cursor_row: u32,
    pub cursor_col: u32,
    /// Selection anchor `(row, col)`, armed by the first
    /// Shift+movement at the pre-move cursor cell. `None` means no
    /// selection; plain (unshifted) movement collapses back to `None`.
    pub anchor: Option<(u32, u32)>,
}

/// Scroll side-effect requested by a copy-mode movement: the cursor
/// hit the top/bottom edge of the visible screen, so the vt100 view
/// should move through scrollback instead of the cursor moving.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CopyModeScroll {
    None,
    Up(usize),
    Down(usize),
}

impl CopyModeState {
    /// Apply a movement key to the cursor. `max_row` / `max_col` are
    /// the last valid cursor cell (content size − 1); `page` is the
    /// PageUp/PageDown scroll amount (content height). Returns the
    /// scroll the caller should apply to the pane's vt100 view.
    ///
    /// Shift+movement extends the selection from the anchor (arming
    /// it at the pre-move cursor on first use); movement without
    /// Shift collapses the anchor. Non-movement keys are a no-op.
    pub fn apply_move(
        &mut self,
        code: KeyCode,
        shift: bool,
        max_row: u32,
        max_col: u32,
        page: usize,
    ) -> CopyModeScroll {
        if shift {
            if self.anchor.is_none() {
                self.anchor = Some((self.cursor_row, self.cursor_col));
            }
        } else {
            self.anchor = None;
        }
        // Clamp first in case the pane shrank since the last key.
        self.cursor_row = self.cursor_row.min(max_row);
        self.cursor_col = self.cursor_col.min(max_col);
        match code {
            KeyCode::Up => {
                if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    CopyModeScroll::None
                } else {
                    CopyModeScroll::Up(1)
                }
            }
            KeyCode::Down => {
                if self.cursor_row < max_row {
                    self.cursor_row += 1;
                    CopyModeScroll::None
                } else {
                    CopyModeScroll::Down(1)
                }
            }
            KeyCode::Left => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
                CopyModeScroll::None
            }
            KeyCode::Right => {
                self.cursor_col = (self.cursor_col + 1).min(max_col);
                CopyModeScroll::None
            }
            KeyCode::Home => {
                self.cursor_col = 0;
                CopyModeScroll::None
            }
            KeyCode::End => {
                self.cursor_col = max_col;
                CopyModeScroll::None
            }
            KeyCode::PageUp => CopyModeScroll::Up(page),
            KeyCode::PageDown => CopyModeScroll::Down(page),
            _ => CopyModeScroll::None,
        }
    }

    /// Shift the selection anchor to track content that moved on
    /// screen because the view scrolled by `moved` lines (`up` =
    /// deeper into scrollback, content slides down). Keeps the
    /// selection glued to the same text while extending past the
    /// screen edge, clamped to the visible area.
    pub fn shift_anchor_for_scroll(&mut self, moved: usize, up: bool, max_row: u32) {
        if let Some((row, col)) = self.anchor {
            let row = if up {
                row.saturating_add(moved as u32).min(max_row)
            } else {
                row.saturating_sub(moved as u32)
            };
            self.anchor = Some((row, col));
        }
    }
}

impl TextSelection {
    /// Get normalized (top-left to bottom-right) selection range.
    pub fn normalized(&self) -> (u32, u32, u32, u32) {
        if self.start_row < self.end_row
            || (self.start_row == self.end_row && self.start_col <= self.end_col)
        {
            (self.start_row, self.start_col, self.end_row, self.end_col)
        } else {
            (self.end_row, self.end_col, self.start_row, self.start_col)
        }
    }

    /// Check if a cell is within the selection.
    pub fn contains(&self, row: u32, col: u32) -> bool {
        let (sr, sc, er, ec) = self.normalized();
        if row < sr || row > er {
            return false;
        }
        if row == sr && row == er {
            return col >= sc && col <= ec;
        }
        if row == sr {
            return col >= sc;
        }
        if row == er {
            return col <= ec;
        }
        true
    }
}
