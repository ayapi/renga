use super::super::*;

fn state(row: u32, col: u32) -> CopyModeState {
    CopyModeState {
        pane_id: 1,
        cursor_row: row,
        cursor_col: col,
        anchor: None,
    }
}

// -- plain movement ----------------------------------------------

#[test]
fn arrows_move_cursor_within_content() {
    let mut cm = state(2, 3);
    assert_eq!(
        cm.apply_move(KeyCode::Up, false, 9, 9, 10),
        CopyModeScroll::None
    );
    assert_eq!((cm.cursor_row, cm.cursor_col), (1, 3));
    assert_eq!(
        cm.apply_move(KeyCode::Down, false, 9, 9, 10),
        CopyModeScroll::None
    );
    assert_eq!((cm.cursor_row, cm.cursor_col), (2, 3));
    assert_eq!(
        cm.apply_move(KeyCode::Left, false, 9, 9, 10),
        CopyModeScroll::None
    );
    assert_eq!((cm.cursor_row, cm.cursor_col), (2, 2));
    assert_eq!(
        cm.apply_move(KeyCode::Right, false, 9, 9, 10),
        CopyModeScroll::None
    );
    assert_eq!((cm.cursor_row, cm.cursor_col), (2, 3));
}

#[test]
fn horizontal_movement_saturates_at_line_ends() {
    let mut cm = state(0, 0);
    cm.apply_move(KeyCode::Left, false, 9, 9, 10);
    assert_eq!(cm.cursor_col, 0, "Left at col 0 must not wrap or underflow");

    let mut cm = state(0, 9);
    cm.apply_move(KeyCode::Right, false, 9, 9, 10);
    assert_eq!(cm.cursor_col, 9, "Right at max_col must clamp");
}

#[test]
fn home_end_jump_to_line_edges() {
    let mut cm = state(4, 5);
    cm.apply_move(KeyCode::Home, false, 9, 9, 10);
    assert_eq!(cm.cursor_col, 0);
    cm.apply_move(KeyCode::End, false, 9, 9, 10);
    assert_eq!(cm.cursor_col, 9);
}

// -- edge scrolling ----------------------------------------------

#[test]
fn up_at_top_row_requests_scrollback() {
    let mut cm = state(0, 3);
    assert_eq!(
        cm.apply_move(KeyCode::Up, false, 9, 9, 10),
        CopyModeScroll::Up(1)
    );
    assert_eq!(
        cm.cursor_row, 0,
        "cursor pins to the top row while the view scrolls"
    );
}

#[test]
fn down_at_bottom_row_requests_scroll_toward_live() {
    let mut cm = state(9, 3);
    assert_eq!(
        cm.apply_move(KeyCode::Down, false, 9, 9, 10),
        CopyModeScroll::Down(1)
    );
    assert_eq!(cm.cursor_row, 9);
}

#[test]
fn page_keys_request_page_scroll_without_moving_cursor() {
    let mut cm = state(4, 3);
    assert_eq!(
        cm.apply_move(KeyCode::PageUp, false, 9, 9, 10),
        CopyModeScroll::Up(10)
    );
    assert_eq!(
        cm.apply_move(KeyCode::PageDown, false, 9, 9, 10),
        CopyModeScroll::Down(10)
    );
    assert_eq!((cm.cursor_row, cm.cursor_col), (4, 3));
}

// -- selection anchor --------------------------------------------

#[test]
fn shift_move_arms_anchor_at_pre_move_cursor() {
    let mut cm = state(2, 3);
    cm.apply_move(KeyCode::Right, true, 9, 9, 10);
    assert_eq!(
        cm.anchor,
        Some((2, 3)),
        "anchor must be the cell before the move"
    );
    assert_eq!((cm.cursor_row, cm.cursor_col), (2, 4));
}

#[test]
fn continued_shift_moves_keep_original_anchor() {
    let mut cm = state(2, 3);
    cm.apply_move(KeyCode::Right, true, 9, 9, 10);
    cm.apply_move(KeyCode::Down, true, 9, 9, 10);
    assert_eq!(cm.anchor, Some((2, 3)));
    assert_eq!((cm.cursor_row, cm.cursor_col), (3, 4));
}

#[test]
fn unshifted_move_collapses_anchor() {
    let mut cm = state(2, 3);
    cm.apply_move(KeyCode::Right, true, 9, 9, 10);
    assert!(cm.anchor.is_some());
    cm.apply_move(KeyCode::Left, false, 9, 9, 10);
    assert_eq!(cm.anchor, None, "plain movement must drop the selection");
}

// -- resize resilience -------------------------------------------

#[test]
fn cursor_clamps_when_pane_shrinks() {
    // Cursor was at (8, 8) in a 10×10 pane; pane shrank to 5×5
    // before the next key. The move must clamp into the new area
    // instead of leaving the cursor (and selection end) out of
    // range.
    let mut cm = state(8, 8);
    cm.apply_move(KeyCode::Down, false, 4, 4, 5);
    assert!(cm.cursor_row <= 4 && cm.cursor_col <= 4);
}

// -- anchor tracking across view scroll --------------------------

#[test]
fn scroll_up_shifts_anchor_down_to_track_content() {
    // View scrolled 1 line into history: content slides down one
    // row on screen, so the anchor row must grow by 1 to stay on
    // the same text.
    let mut cm = state(0, 3);
    cm.anchor = Some((4, 2));
    cm.shift_anchor_for_scroll(1, true, 9);
    assert_eq!(cm.anchor, Some((5, 2)));
}

#[test]
fn scroll_down_shifts_anchor_up_and_saturates() {
    let mut cm = state(9, 3);
    cm.anchor = Some((1, 2));
    cm.shift_anchor_for_scroll(3, false, 9);
    assert_eq!(
        cm.anchor,
        Some((0, 2)),
        "anchor clamps at the top instead of underflowing"
    );
}

#[test]
fn scroll_up_clamps_anchor_at_bottom_row() {
    let mut cm = state(0, 3);
    cm.anchor = Some((8, 2));
    cm.shift_anchor_for_scroll(5, true, 9);
    assert_eq!(
        cm.anchor,
        Some((9, 2)),
        "anchor clamps at max_row when content scrolls past"
    );
}

#[test]
fn zero_actual_scroll_leaves_anchor_untouched() {
    // vt100 clamped the scroll (already at the end of scrollback):
    // moved == 0 must be a no-op.
    let mut cm = state(0, 3);
    cm.anchor = Some((4, 2));
    cm.shift_anchor_for_scroll(0, true, 9);
    assert_eq!(cm.anchor, Some((4, 2)));
}
