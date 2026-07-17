use super::super::*;

#[test]
fn test_layout_single_pane() {
    let layout = LayoutNode::Leaf { pane_id: 1 };
    assert_eq!(layout.pane_count(), 1);
    assert_eq!(layout.collect_pane_ids(), vec![1]);
}

#[test]
fn test_layout_split_vertical() {
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Vertical, false);
    assert_eq!(layout.pane_count(), 2);
    assert_eq!(layout.collect_pane_ids(), vec![1, 2]);
}

#[test]
fn test_layout_split_horizontal() {
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Horizontal, false);
    assert_eq!(layout.pane_count(), 2);
}

#[test]
fn test_layout_nested_split() {
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Vertical, false);
    layout.split_pane_with_position(1, 3, SplitDirection::Horizontal, false);
    assert_eq!(layout.pane_count(), 3);
    assert_eq!(layout.collect_pane_ids(), vec![1, 3, 2]);
}

#[test]
fn test_layout_remove_pane() {
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Vertical, false);
    layout.remove_pane(2);
    assert_eq!(layout.pane_count(), 1);
    assert_eq!(layout.collect_pane_ids(), vec![1]);
}

#[test]
fn test_layout_remove_first_pane() {
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Vertical, false);
    layout.remove_pane(1);
    assert_eq!(layout.collect_pane_ids(), vec![2]);
}

#[test]
fn test_calculate_rects_vertical() {
    let layout = LayoutNode::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.5,
        first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
        second: Box::new(LayoutNode::Leaf { pane_id: 2 }),
    };
    let rects = layout.calculate_rects(Rect::new(0, 0, 100, 50));
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0], (1, Rect::new(0, 0, 50, 50)));
    assert_eq!(rects[1], (2, Rect::new(50, 0, 50, 50)));
}

#[test]
fn test_calculate_rects_horizontal() {
    let layout = LayoutNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
        second: Box::new(LayoutNode::Leaf { pane_id: 2 }),
    };
    let rects = layout.calculate_rects(Rect::new(0, 0, 100, 50));
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0], (1, Rect::new(0, 0, 100, 25)));
    assert_eq!(rects[1], (2, Rect::new(0, 25, 100, 25)));
}

#[test]
fn test_focus_cycling() {
    let ids = [1, 2, 3];
    assert_eq!(1 % ids.len(), 1);
    assert_eq!((2 + 1) % ids.len(), 0);
}

// ─── resolve_pane_ref_impl (Phase 3 Step 3.2) ────────────

fn mk_ids(ids: &[usize]) -> HashSet<usize> {
    ids.iter().copied().collect()
}

#[test]
fn resolve_focused_returns_focused_id_when_known() {
    let names = HashMap::new();
    let ids = mk_ids(&[1, 2, 3]);
    assert_eq!(
        resolve_pane_ref_impl(&PaneRef::Focused, &names, &ids, 2),
        Some(2)
    );
}

#[test]
fn resolve_focused_returns_none_when_focus_stale() {
    let names = HashMap::new();
    let ids = mk_ids(&[1, 3]);
    assert_eq!(
        resolve_pane_ref_impl(&PaneRef::Focused, &names, &ids, 2),
        None
    );
}

#[test]
fn resolve_by_id_returns_id_when_known() {
    let names = HashMap::new();
    let ids = mk_ids(&[1, 2, 3]);
    assert_eq!(
        resolve_pane_ref_impl(&PaneRef::Id(3), &names, &ids, 1),
        Some(3)
    );
}

#[test]
fn resolve_by_id_returns_none_when_unknown() {
    let names = HashMap::new();
    let ids = mk_ids(&[1, 2]);
    assert_eq!(
        resolve_pane_ref_impl(&PaneRef::Id(99), &names, &ids, 1),
        None
    );
}

#[test]
fn resolve_by_name_returns_id_when_registered() {
    let mut names = HashMap::new();
    names.insert("engineering".to_string(), 7);
    let ids = mk_ids(&[1, 7]);
    assert_eq!(
        resolve_pane_ref_impl(&PaneRef::Name("engineering".into()), &names, &ids, 1),
        Some(7)
    );
}

#[test]
fn resolve_by_name_returns_none_when_unregistered() {
    let names = HashMap::new();
    let ids = mk_ids(&[1, 7]);
    assert_eq!(
        resolve_pane_ref_impl(&PaneRef::Name("missing".into()), &names, &ids, 1),
        None
    );
}

#[test]
fn resolve_by_name_returns_none_when_pane_closed() {
    // Name still registered but the pane has been removed — the
    // dangling entry must not resolve to a ghost id.
    let mut names = HashMap::new();
    names.insert("engineering".to_string(), 7);
    let ids = mk_ids(&[1]); // 7 has been closed
    assert_eq!(
        resolve_pane_ref_impl(&PaneRef::Name("engineering".into()), &names, &ids, 1),
        None
    );
}

// ─── apply_layout integration (Phase 2 review fix) ────────
//
// These tests spawn real PTYs through App::new / Pane::new_with_cwd
// because apply_layout drives split_focused_pane, which unavoidably
// creates child shell processes. On a dev machine this costs a few
// milliseconds per pane; in CI it's measurable but acceptable for a
// handful of tests. Each test calls `app.shutdown()` at the end so
// the spawned shells don't linger.

// ─── split_pane_with_position (Issue #245) ──────────────────────

#[test]
fn split_with_position_first_places_new_pane_in_first_slot() {
    // new_pane_first = true → new pane lands in the first (top / left)
    // slot. Vertical layout, ratio 0.5: at area (0,0,100,50) the new
    // pane should occupy the left half.
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Vertical, true);
    let rects = layout.calculate_rects(Rect::new(0, 0, 100, 50));
    assert_eq!(rects.len(), 2);
    assert_eq!(
        rects[0],
        (2, Rect::new(0, 0, 50, 50)),
        "new pane (2) is first"
    );
    assert_eq!(
        rects[1],
        (1, Rect::new(50, 0, 50, 50)),
        "old pane (1) is second"
    );
}

#[test]
fn split_with_position_second_keeps_legacy_placement() {
    // new_pane_first = false must place the new pane in the second
    // (bottom / right) child slot — what the split chords (Alt+D /
    // Alt+E, formerly Ctrl+D / Ctrl+E) have always done. Regression guard for the historical placement.
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Vertical, false);
    let rects = layout.calculate_rects(Rect::new(0, 0, 100, 50));
    assert_eq!(
        rects[0],
        (1, Rect::new(0, 0, 50, 50)),
        "old pane stays first"
    );
    assert_eq!(
        rects[1],
        (2, Rect::new(50, 0, 50, 50)),
        "new pane is second"
    );
}

#[test]
fn split_with_position_horizontal_top_places_new_pane_top() {
    // Top-edge double-click semantics: horizontal split,
    // new_pane_first = true → new pane occupies the top half.
    let mut layout = LayoutNode::Leaf { pane_id: 1 };
    layout.split_pane_with_position(1, 2, SplitDirection::Horizontal, true);
    let rects = layout.calculate_rects(Rect::new(0, 0, 100, 50));
    assert_eq!(rects[0], (2, Rect::new(0, 0, 100, 25)));
    assert_eq!(rects[1], (1, Rect::new(0, 25, 100, 25)));
}

// ─── split_boundaries span (Issue #247) ─────────────────────────

#[test]
fn split_boundaries_flat_vertical_spans_full_height() {
    // A single top-level vertical split: the divider sits at x = 50
    // and spans every row of the area.
    let layout = LayoutNode::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.5,
        first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
        second: Box::new(LayoutNode::Leaf { pane_id: 2 }),
    };
    let bounds = layout.split_boundaries(Rect::new(0, 0, 100, 50));
    assert_eq!(bounds.len(), 1);
    assert_eq!(bounds[0].position, 50);
    assert_eq!(bounds[0].direction, SplitDirection::Vertical);
    assert_eq!(bounds[0].span, (0, 50), "vertical divider spans all rows");
    assert!(bounds[0].path.is_empty());
}

#[test]
fn split_boundaries_nested_divider_spans_only_its_subregion() {
    // Top-level HORIZONTAL split: full-width top pane (1) over a
    // bottom subtree that is itself a VERTICAL split (2 | 3).
    //   1 → (0,0,100,25)
    //   2 → (0,25,50,25)   3 → (50,25,50,25)
    // The nested vertical divider at x = 50 must report a span of
    // rows 25..50 — NOT the whole 0..50 height. Before #247 the
    // hit-test assumed full height, so a click at (50, 10) — inside
    // the top pane — wrongly registered as a boundary press.
    let layout = LayoutNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(LayoutNode::Leaf { pane_id: 1 }),
        second: Box::new(LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { pane_id: 2 }),
            second: Box::new(LayoutNode::Leaf { pane_id: 3 }),
        }),
    };
    let bounds = layout.split_boundaries(Rect::new(0, 0, 100, 50));
    assert_eq!(bounds.len(), 2);

    let top = bounds.iter().find(|b| b.path.is_empty()).expect("root");
    assert_eq!(top.direction, SplitDirection::Horizontal);
    assert_eq!(top.position, 25);
    assert_eq!(top.span, (0, 100), "horizontal divider spans all cols");

    let nested = bounds
        .iter()
        .find(|b| b.path == vec![true])
        .expect("nested vertical divider");
    assert_eq!(nested.direction, SplitDirection::Vertical);
    assert_eq!(nested.position, 50);
    assert_eq!(
        nested.span,
        (25, 50),
        "nested divider must span only the bottom subregion"
    );
}
