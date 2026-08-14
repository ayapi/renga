use super::super::*;

// -- pane_local_coords / pane_local_coords_clamped ---------------

#[test]
fn pane_local_coords_rejects_border_clicks() {
    // A 10×5 pane at origin (2, 3): border at col 2 / col 11 /
    // row 3 / row 7. A click on any border cell must decline to
    // forward so the caller can fall through to the border-drag
    // handler instead.
    let rect = Rect::new(2, 3, 10, 5);
    assert!(pane_local_coords(rect, 2, 5).is_none(), "left border");
    assert!(pane_local_coords(rect, 11, 5).is_none(), "right border");
    assert!(pane_local_coords(rect, 5, 3).is_none(), "top border");
    assert!(pane_local_coords(rect, 5, 7).is_none(), "bottom border");
}

#[test]
fn pane_local_coords_translates_to_content_0_origin() {
    // Pane outer at (2, 3), content starts at (3, 4). A click at
    // screen (3, 4) must land on content (0, 0); (10, 6) maps
    // to (7, 2).
    let rect = Rect::new(2, 3, 10, 5);
    assert_eq!(pane_local_coords(rect, 3, 4), Some((0, 0)));
    assert_eq!(pane_local_coords(rect, 10, 6), Some((7, 2)));
}

#[test]
fn pane_local_coords_clamped_stays_inside_content() {
    // Clamp is used on Drag/Up where the cursor may wander off-
    // pane. Ensure clamp never produces an out-of-bounds cell.
    let rect = Rect::new(2, 3, 10, 5);
    // Cursor well to the right of the pane — should pin to the
    // last content column (width - 2 = 8 inner cells, 0..=7).
    assert_eq!(pane_local_coords_clamped(rect, 50, 50), (7, 2));
    // Cursor above/left of the pane — should pin to (0, 0).
    assert_eq!(pane_local_coords_clamped(rect, 0, 0), (0, 0));
    // Cursor inside — untouched.
    assert_eq!(pane_local_coords_clamped(rect, 5, 5), (2, 1));
}

#[test]
fn pane_local_coords_rejects_rects_too_small_for_content() {
    // A 2×2 or narrower rect has no interior after stripping the
    // 1-cell border. Codex review flagged that the pre-fix version
    // underflowed with `rect.width == 1`; the guard keeps such a
    // press from ever reaching the forward path.
    for (w, h) in [(0, 5), (1, 5), (2, 5), (5, 0), (5, 1), (5, 2)] {
        let rect = Rect::new(2, 3, w, h);
        assert!(
            pane_local_coords(rect, 3, 4).is_none(),
            "{}×{} rect must be rejected before the arithmetic fires",
            w,
            h
        );
    }
}

#[test]
fn pane_local_coords_survives_extreme_u16_origins() {
    // `rect.x = u16::MAX - 5` means `rect.x + rect.width` would
    // overflow in unchecked arithmetic. Saturating math keeps this
    // from panicking in debug builds.
    let rect = Rect::new(u16::MAX - 5, 0, 10, 5);
    // The call must return, not panic. Result content is
    // secondary — whatever it yields, it yields safely.
    let _ = pane_local_coords(rect, u16::MAX - 3, 2);
    let _ = pane_local_coords_clamped(rect, u16::MAX, u16::MAX);
}

// -- mouse_forward_disabled env gate -----------------------------

#[test]
fn mouse_forward_disabled_reads_env_var() {
    // Serialize against the global env so parallel tests don't
    // see each other's values. We only toggle within this test.
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();

    std::env::remove_var("RENGA_DISABLE_MOUSE_FORWARD");
    assert!(!mouse_forward_disabled(), "unset → false");

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "1");
    assert!(mouse_forward_disabled(), "\"1\" → true");

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "0");
    assert!(
        !mouse_forward_disabled(),
        "\"0\" must be treated as opt-in-off, matching the wheel-handler convention"
    );

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "");
    assert!(!mouse_forward_disabled(), "empty string → false");

    std::env::set_var("RENGA_DISABLE_MOUSE_FORWARD", "yes");
    assert!(
        mouse_forward_disabled(),
        "any non-empty non-\"0\" value → true (permissive)"
    );

    std::env::remove_var("RENGA_DISABLE_MOUSE_FORWARD");
}

// ─── detect_outer_edge (Issue #245) ────────────────────────────

/// A single 100×50 pane filling the whole workspace. Every test in
/// this group uses this layout unless noted.
fn single_pane_rects() -> Vec<(usize, Rect)> {
    vec![(1, Rect::new(0, 0, 100, 50))]
}

#[test]
fn detect_outer_edge_returns_each_side_for_single_pane() {
    // A solo pane is the simplest case: all four outer edges belong
    // to pane 1 and corner cells must be rejected.
    let rects = single_pane_rects();
    let area = Rect::new(0, 0, 100, 50);

    // Mid-edge of each side: detection must classify the side and
    // return the only pane id.
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 0),
        Some((EdgeSide::Top, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 49),
        Some((EdgeSide::Bottom, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 0, 25),
        Some((EdgeSide::Left, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 25),
        Some((EdgeSide::Right, 1))
    );
}

#[test]
fn detect_outer_edge_rejects_corner_cells() {
    // Issue §3.5 v1: corners are ambiguous (two sides meet) so they
    // must not trigger a split.
    let rects = single_pane_rects();
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(detect_outer_edge(area, &rects, 0, 0), None, "top-left");
    assert_eq!(detect_outer_edge(area, &rects, 99, 0), None, "top-right");
    assert_eq!(detect_outer_edge(area, &rects, 0, 49), None, "bottom-left");
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 49),
        None,
        "bottom-right"
    );
}

#[test]
fn detect_outer_edge_rejects_inner_cells() {
    // Interior PTY cells must never look like an edge click.
    let rects = single_pane_rects();
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(detect_outer_edge(area, &rects, 50, 25), None);
    assert_eq!(detect_outer_edge(area, &rects, 1, 1), None);
    assert_eq!(detect_outer_edge(area, &rects, 98, 48), None);
}

#[test]
fn detect_outer_edge_rejects_shared_internal_boundary_cells() {
    // Two panes split vertically at x=50. The shared internal column
    // (x=50, the boundary between pane 1's right border and pane 2's
    // left border) is NOT an outer edge — only the workspace's true
    // outer rim is. Clicks on x=50 must return None so the
    // resize-drag boundary handler (which runs first in real code)
    // keeps owning that column.
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    // A point inside the shared boundary column but in the interior
    // (not on a row at the top/bottom outer edge) is fully internal.
    assert_eq!(detect_outer_edge(area, &rects, 50, 25), None);
    assert_eq!(detect_outer_edge(area, &rects, 49, 25), None);
}

#[test]
fn detect_outer_edge_nested_layout_does_not_overreject() {
    // Codex round 2: the boundary check must consider whether the
    // internal boundary actually spans the clicked outer row/col.
    // Layout: full-width top pane, bottom split left/right at col 50.
    //   A (0,0,100,25)
    //   B (0,25,50,25)  |  C (50,25,50,25)
    // A click at (50, 0) is on the top outer edge of pane A. The
    // x=50 vertical boundary only exists in the lower half, so it
    // must NOT block the click from registering as A's top edge.
    let rects = vec![
        (1, Rect::new(0, 0, 100, 25)),
        (2, Rect::new(0, 25, 50, 25)),
        (3, Rect::new(50, 25, 50, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 0),
        Some((EdgeSide::Top, 1)),
        "nested-bottom boundary at col 50 must not over-reject top-edge click"
    );
    // Symmetric: full-width bottom pane, top split left/right.
    let rects = vec![
        (1, Rect::new(0, 0, 50, 25)),
        (2, Rect::new(50, 0, 50, 25)),
        (3, Rect::new(0, 25, 100, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 49),
        Some((EdgeSide::Bottom, 3)),
        "nested-top boundary at col 50 must not over-reject bottom-edge click"
    );
}

#[test]
fn detect_outer_edge_rejects_outer_edge_x_internal_boundary_intersection() {
    // The intersection of an outer edge and an internal boundary is
    // ambiguous: cell (50, 0) in a vertical split sits on both the
    // top outer edge AND the internal x=50 boundary. In real flow
    // the boundary-resize hit-test runs first and consumes it, so
    // this is a defense-in-depth guard — but the helper's contract
    // ("reject shared boundaries") must hold standalone too.
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 0),
        None,
        "top edge ∩ internal vertical boundary"
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 49),
        None,
        "bottom edge ∩ internal vertical boundary"
    );
    // Horizontal split rejects left/right edge ∩ internal horizontal
    // boundary symmetrically.
    let rects = vec![
        (1, Rect::new(0, 0, 100, 25)),
        (2, Rect::new(0, 25, 100, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 0, 25),
        None,
        "left edge ∩ internal horizontal boundary"
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 25),
        None,
        "right edge ∩ internal horizontal boundary"
    );
}

#[test]
fn detect_outer_edge_picks_correct_pane_in_multi_layout() {
    // Vertical split: top-edge click on the left half must target
    // pane 1, top-edge click on the right half must target pane 2.
    // Same for the bottom edge. Left/right outer edges respectively
    // belong to pane 1 / pane 2.
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_outer_edge(area, &rects, 25, 0),
        Some((EdgeSide::Top, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 75, 0),
        Some((EdgeSide::Top, 2))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 25, 49),
        Some((EdgeSide::Bottom, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 75, 49),
        Some((EdgeSide::Bottom, 2))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 0, 25),
        Some((EdgeSide::Left, 1))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 25),
        Some((EdgeSide::Right, 2))
    );
}

#[test]
fn detect_outer_edge_offset_origin_workspace() {
    // The workspace doesn't always start at (0, 0); a side bar or
    // status bar can offset the pane area. detect_outer_edge must
    // anchor off `pane_area`, not absolute coordinates.
    let rects = vec![(7, Rect::new(20, 3, 80, 40))];
    let area = Rect::new(20, 3, 80, 40);
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 3),
        Some((EdgeSide::Top, 7))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 50, 42),
        Some((EdgeSide::Bottom, 7))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 20, 20),
        Some((EdgeSide::Left, 7))
    );
    assert_eq!(
        detect_outer_edge(area, &rects, 99, 20),
        Some((EdgeSide::Right, 7))
    );
    // Cell on the workspace edge but BEYOND the pane area: not an
    // edge click for this layout.
    assert_eq!(detect_outer_edge(area, &rects, 5, 1), None);
}

// ─── split_intent_for_edge (Issue #245) ────────────────────────

#[test]
fn split_intent_top_left_place_new_pane_first() {
    // Top and Left clicks must spawn the new pane in the first
    // (top / left) slot so the clicked edge becomes the new pane's
    // edge — the spec's "click on top → new pane appears on top".
    assert_eq!(
        split_intent_for_edge(EdgeSide::Top),
        (SplitDirection::Horizontal, true)
    );
    assert_eq!(
        split_intent_for_edge(EdgeSide::Left),
        (SplitDirection::Vertical, true)
    );
}

#[test]
fn split_intent_bottom_right_place_new_pane_second() {
    // Bottom and Right clicks place the new pane in the trailing
    // slot, matching the historical split-chord placement (Alt+D /
    // Alt+E, formerly Ctrl+D / Ctrl+E).
    assert_eq!(
        split_intent_for_edge(EdgeSide::Bottom),
        (SplitDirection::Horizontal, false)
    );
    assert_eq!(
        split_intent_for_edge(EdgeSide::Right),
        (SplitDirection::Vertical, false)
    );
}

// ─── detect_shared_boundary (Issue #247) ────────────────────────

#[test]
fn detect_shared_boundary_vertical_targets_left_leaf() {
    // [1 | 2] split vertically at x = 50. The divider occupies the
    // two columns {49, 50}. A double-click anywhere on it splits the
    // LEFT leaf (1) to its right, so the new pane lands between 1 and
    // 2. Direction is Vertical, new_pane_first = false.
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_shared_boundary(area, &rects, 50, 25),
        Some((SplitDirection::Vertical, 1, false))
    );
    assert_eq!(
        detect_shared_boundary(area, &rects, 49, 25),
        Some((SplitDirection::Vertical, 1, false)),
        "the left border column of the divider counts too"
    );
}

#[test]
fn detect_shared_boundary_horizontal_targets_top_leaf() {
    // [1 / 2] split horizontally at y = 25. The divider occupies rows
    // {24, 25}. A double-click splits the TOP leaf (1) downward.
    let rects = vec![
        (1, Rect::new(0, 0, 100, 25)),
        (2, Rect::new(0, 25, 100, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_shared_boundary(area, &rects, 50, 25),
        Some((SplitDirection::Horizontal, 1, false))
    );
    assert_eq!(
        detect_shared_boundary(area, &rects, 50, 24),
        Some((SplitDirection::Horizontal, 1, false))
    );
}

#[test]
fn detect_shared_boundary_rejects_outer_rim() {
    // The workspace's own outer edges are NOT shared boundaries —
    // those belong to the outer-edge double-click path (#245).
    let rects = vec![(1, Rect::new(0, 0, 50, 50)), (2, Rect::new(50, 0, 50, 50))];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(
        detect_shared_boundary(area, &rects, 0, 25),
        None,
        "left rim"
    );
    assert_eq!(
        detect_shared_boundary(area, &rects, 99, 25),
        None,
        "right rim"
    );
    assert_eq!(detect_shared_boundary(area, &rects, 25, 0), None, "top rim");
    assert_eq!(
        detect_shared_boundary(area, &rects, 25, 49),
        None,
        "bottom rim"
    );
}

#[test]
fn detect_shared_boundary_nested_hit_test_respects_span() {
    // Full-width top pane (1) over a bottom left/right split (2 | 3):
    //   1 → (0,0,100,25)
    //   2 → (0,25,50,25)   3 → (50,25,50,25)
    // The nested vertical divider lives only in the bottom half.
    let rects = vec![
        (1, Rect::new(0, 0, 100, 25)),
        (2, Rect::new(0, 25, 50, 25)),
        (3, Rect::new(50, 25, 50, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    // Click at column 50 but in the TOP pane's rows: not a boundary.
    // (This is exactly the gap #246 left open and #247 closes.)
    assert_eq!(
        detect_shared_boundary(area, &rects, 50, 10),
        None,
        "col 50 inside the top pane is not a divider"
    );
    // Click at column 50 in the BOTTOM rows: splits the bottom-left
    // leaf (2) to its right.
    assert_eq!(
        detect_shared_boundary(area, &rects, 50, 40),
        Some((SplitDirection::Vertical, 2, false))
    );
}

#[test]
fn detect_shared_boundary_rejects_ambiguous_junction() {
    // Where a vertical and a horizontal divider cross, the split
    // direction is ambiguous, so the helper declines (like a corner
    // cell) and the click stays a resize-drag.
    //   1 → (0,0,100,25) full-width top
    //   2 → (0,25,50,25)   3 → (50,25,50,25)
    // Cell (50, 25) sits on both pane 1's bottom edge (horizontal
    // divider) and the 2|3 vertical divider.
    let rects = vec![
        (1, Rect::new(0, 0, 100, 25)),
        (2, Rect::new(0, 25, 50, 25)),
        (3, Rect::new(50, 25, 50, 25)),
    ];
    let area = Rect::new(0, 0, 100, 50);
    assert_eq!(detect_shared_boundary(area, &rects, 50, 25), None);
}

// ─── handle_mouse_event boundary click vs drag (Issue #247) ─────
//
// These drive the real event handler, which spawns PTYs on split, so
// each test calls `app.shutdown()` at the end. A two-pane vertical
// layout is set up by hand: split the focused pane, then publish the
// rects the renderer would have produced into `last_pane_rects` so the
// pointer code can hit-test against them.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn boundary_mouse(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    }
}

/// Build an app whose active tab holds `[A | B]` split vertically over
/// a 100×40 area, with `last_pane_rects` published. Returns
/// `(app, a_id, b_id)`; the divider sits at column 50.
fn two_pane_vertical_app() -> (App, usize, usize) {
    let mut app = App::new(40, 100).expect("App::new");
    let a_id = app.ws().focused_pane_id;
    app.split_focused_pane_with_position(SplitDirection::Vertical, false, None)
        .expect("split succeeds")
        .expect("split not refused");
    let b_id = app.ws().focused_pane_id;
    assert_ne!(a_id, b_id);
    let area = Rect::new(0, 0, 100, 40);
    let rects = app.ws().layout.calculate_rects(area);
    app.ws_mut().last_pane_rects = rects;
    (app, a_id, b_id)
}

fn make_scrollbar_visible(app: &App, pane_id: usize) {
    app.ws()
        .panes
        .get(&pane_id)
        .expect("pane exists")
        .total_scrollback
        .store(1_000, std::sync::atomic::Ordering::Relaxed);
}

fn pane_rect(app: &App, pane_id: usize) -> Rect {
    app.ws()
        .last_pane_rects
        .iter()
        .find_map(|&(id, rect)| (id == pane_id).then_some(rect))
        .expect("pane rect exists")
}

#[test]
fn visible_scrollbar_on_left_split_wins_over_resize_gesture() {
    let (mut app, a_id, _b_id) = two_pane_vertical_app();
    make_scrollbar_visible(&app, a_id);
    let rect = pane_rect(&app, a_id);
    let scrollbar_col = rect.x + rect.width - 1;

    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        scrollbar_col,
        20,
    ));

    assert!(matches!(
        app.dragging,
        Some(DragTarget::Scrollbar(id, _)) if id == a_id
    ));
    assert_eq!(app.ws().layout.pane_count(), 2);
    app.shutdown();
}

#[test]
fn repeated_visible_scrollbar_click_does_not_split_outer_edge() {
    let (mut app, _a_id, b_id) = two_pane_vertical_app();
    make_scrollbar_visible(&app, b_id);
    let rect = pane_rect(&app, b_id);
    let scrollbar_col = rect.x + rect.width - 1;

    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        scrollbar_col,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Up(MouseButton::Left),
        scrollbar_col,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        scrollbar_col,
        20,
    ));

    assert_eq!(app.ws().layout.pane_count(), 2);
    assert!(matches!(
        app.dragging,
        Some(DragTarget::Scrollbar(id, _)) if id == b_id
    ));
    app.shutdown();
}

#[test]
fn hidden_scrollbar_does_not_claim_right_border_click() {
    let (mut app, _a_id, b_id) = two_pane_vertical_app();
    app.ws()
        .panes
        .get(&b_id)
        .unwrap()
        .total_scrollback
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let rect = pane_rect(&app, b_id);
    let right_border_col = rect.x + rect.width - 1;

    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        right_border_col,
        20,
    ));

    assert!(!matches!(app.dragging, Some(DragTarget::Scrollbar(..))));
    assert!(app.last_edge_click.is_some());
    assert_eq!(app.ws().layout.pane_count(), 2);
    app.shutdown();
}

#[test]
fn exited_pane_does_not_claim_invisible_scrollbar() {
    let (mut app, a_id, _b_id) = two_pane_vertical_app();
    make_scrollbar_visible(&app, a_id);
    app.ws_mut().panes.get_mut(&a_id).unwrap().exited = true;
    let rect = pane_rect(&app, a_id);
    let right_border_col = rect.x + rect.width - 1;

    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        right_border_col,
        20,
    ));

    assert!(!matches!(app.dragging, Some(DragTarget::Scrollbar(..))));
    assert!(matches!(app.dragging, Some(DragTarget::PaneSplit(..))));
    app.shutdown();
}

#[test]
fn visible_scrollbar_hover_does_not_advertise_split_resize() {
    let (mut app, a_id, _b_id) = two_pane_vertical_app();
    make_scrollbar_visible(&app, a_id);
    let rect = pane_rect(&app, a_id);
    let scrollbar_col = rect.x + rect.width - 1;

    app.handle_mouse_event(boundary_mouse(MouseEventKind::Moved, scrollbar_col, 20));

    assert!(matches!(
        app.hover_border,
        Some(DragTarget::Scrollbar(id, _)) if id == a_id
    ));
    app.shutdown();
}

#[test]
fn boundary_double_click_splits_between_siblings() {
    let (mut app, a_id, b_id) = two_pane_vertical_app();
    assert_eq!(app.ws().layout.pane_count(), 2);

    // First click on the divider arms the timer (and sets up a
    // resize-drag); the release cancels the drag without resizing.
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        50,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Up(MouseButton::Left),
        50,
        20,
    ));
    assert_eq!(
        app.ws().layout.pane_count(),
        2,
        "a single click must not split"
    );

    // Second click within 500 ms → double-click → split the left
    // sibling, dropping the new leaf between A and B.
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        50,
        20,
    ));
    let new_id = app.ws().focused_pane_id;
    assert_eq!(app.ws().layout.pane_count(), 3, "double-click splits");
    assert_eq!(
        app.ws().layout.collect_pane_ids(),
        vec![a_id, new_id, b_id],
        "new leaf is inserted between the two siblings"
    );
    app.shutdown();
}

#[test]
fn boundary_single_click_is_noop() {
    let (mut app, a_id, b_id) = two_pane_vertical_app();
    let ids_before = app.ws().layout.collect_pane_ids();

    // A lone down/up on the divider (no second click, no drag) must
    // leave the layout untouched.
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        50,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Up(MouseButton::Left),
        50,
        20,
    ));

    assert_eq!(app.ws().layout.pane_count(), 2);
    assert_eq!(app.ws().layout.collect_pane_ids(), ids_before);
    assert_eq!(ids_before, vec![a_id, b_id]);
    app.shutdown();
}

#[test]
fn boundary_drag_resizes_and_does_not_split() {
    let (mut app, _a_id, _b_id) = two_pane_vertical_app();

    // Press on the divider, then drag left to x = 30. The drag must
    // resize (ratio → 0.30) and never split.
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        50,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        30,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Up(MouseButton::Left),
        30,
        20,
    ));

    assert_eq!(app.ws().layout.pane_count(), 2, "a drag must not split");
    if let LayoutNode::Split { ratio, .. } = &app.ws().layout {
        assert!(
            (*ratio - 0.30).abs() < 0.01,
            "drag to x=30 over a width-100 area resizes to ~0.30, got {ratio}"
        );
    } else {
        panic!("layout should still be a single split");
    }
    app.shutdown();
}

#[test]
fn boundary_drag_then_click_does_not_phantom_split() {
    // After a resize-drag, the pending double-click candidate must be
    // dropped — otherwise a click on the same cell within 500 ms would
    // be misread as a double-click and split. (Codex review, #247)
    let (mut app, _a_id, _b_id) = two_pane_vertical_app();

    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        50,
        20,
    ));
    // Drag in place (boundary stays at x=50), then release.
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        50,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Up(MouseButton::Left),
        50,
        20,
    ));
    // A follow-up click on the same divider cell must arm a fresh
    // timer, not split.
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        50,
        20,
    ));

    assert_eq!(
        app.ws().layout.pane_count(),
        2,
        "a click after a drag must not phantom-split"
    );
    app.shutdown();
}

#[test]
fn boundary_drag_ratio_is_relative_to_nested_subregion() {
    // Layout [A | [B | C]] over a 100×40 area: a top-level vertical
    // split at x=50, and a nested vertical split (B|C) inside the
    // right half, with its divider at x=75. Dragging the NESTED
    // divider must measure the ratio against the right half's own rect
    // (x∈[50,100)), not the whole pane area — so a drag to x=80 yields
    // (80-50)/50 = 0.60, not the buggy 80/100 = 0.80. (Codex review)
    let mut app = App::new(40, 100).expect("App::new");
    app.split_focused_pane_with_position(SplitDirection::Vertical, false, None)
        .expect("split A|B")
        .expect("not refused");
    app.split_focused_pane_with_position(SplitDirection::Vertical, false, None)
        .expect("split B|C")
        .expect("not refused");
    assert_eq!(app.ws().layout.pane_count(), 3);

    let area = Rect::new(0, 0, 100, 40);
    app.ws_mut().last_pane_rects = app.ws().layout.calculate_rects(area);

    // The nested divider is at x=75; drag it to x=80.
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Down(MouseButton::Left),
        75,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        80,
        20,
    ));
    app.handle_mouse_event(boundary_mouse(
        MouseEventKind::Up(MouseButton::Left),
        80,
        20,
    ));

    assert_eq!(app.ws().layout.pane_count(), 3, "drag must not split");
    // Walk to the nested split's ratio.
    if let LayoutNode::Split { second, .. } = &app.ws().layout {
        if let LayoutNode::Split { ratio, .. } = second.as_ref() {
            assert!(
                (*ratio - 0.60).abs() < 0.01,
                "nested drag should resize relative to the right half, got {ratio}"
            );
        } else {
            panic!("right child should be the nested split");
        }
    } else {
        panic!("root should be a split");
    }
    app.shutdown();
}
