use super::*;

/// Outer edge of the pane area for the double-click split feature.
/// Encodes both the visual side and the post-split placement: clicking
/// Top/Left spawns the new pane on the clicked side (first child);
/// Bottom/Right keeps the historical "new pane on the trailing side"
/// behavior (second child). See Issue #245.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeSide {
    Top,
    Bottom,
    Left,
    Right,
}

/// Decide whether `(col, row)` lies on the outer edge of `pane_area`
/// (the bounding box of all pane rects) and, if so, which leaf pane
/// owns that edge cell. Corner cells (on two outer edges at once) are
/// rejected for v1 because their split direction is ambiguous and the
/// historical click already focuses the corner pane. Cells on shared
/// internal boundaries return `None` here because their `rect.x` /
/// `rect.y` doesn't match `pane_area`'s — those clicks are handled by
/// the resize-drag boundary path that already runs first.
pub(crate) fn detect_outer_edge(
    pane_area: Rect,
    pane_rects: &[(usize, Rect)],
    col: u16,
    row: u16,
) -> Option<(EdgeSide, usize)> {
    if pane_area.width == 0 || pane_area.height == 0 {
        return None;
    }
    // Saturating math keeps detection deterministic at u16 edges
    // (`pane_local_coords` follows the same convention).
    let bottom = pane_area.y.saturating_add(pane_area.height);
    let right = pane_area.x.saturating_add(pane_area.width);
    let on_top = row == pane_area.y;
    let on_bottom = row.saturating_add(1) == bottom;
    let on_left = col == pane_area.x;
    let on_right = col.saturating_add(1) == right;
    let side_count = on_top as u8 + on_bottom as u8 + on_left as u8 + on_right as u8;
    if side_count != 1 {
        // Either not on the outer edge, or on a corner (two sides).
        return None;
    }
    let side = if on_top {
        EdgeSide::Top
    } else if on_bottom {
        EdgeSide::Bottom
    } else if on_left {
        EdgeSide::Left
    } else {
        EdgeSide::Right
    };
    // Reject the intersection of an outer edge and a shared internal
    // boundary, but only when the boundary actually spans the clicked
    // row/col. A nested layout (e.g., top: full-width pane, bottom:
    // split left/right) places an internal vertical boundary only in
    // the lower half — a top-edge click at the same column is still
    // a legitimate outer-edge click on the top pane.
    let v_boundary_at_row = pane_rects.iter().any(|(_, r)| {
        let r_right = r.x.saturating_add(r.width);
        let r_bottom = r.y.saturating_add(r.height);
        let spans_row = row >= r.y && row < r_bottom;
        spans_row
            && ((r.x == col && r.x != pane_area.x)
                || (r_right == col.saturating_add(1) && r_right != right))
    });
    let h_boundary_at_col = pane_rects.iter().any(|(_, r)| {
        let r_right = r.x.saturating_add(r.width);
        let r_bottom = r.y.saturating_add(r.height);
        let spans_col = col >= r.x && col < r_right;
        spans_col
            && ((r.y == row && r.y != pane_area.y)
                || (r_bottom == row.saturating_add(1) && r_bottom != bottom))
    });
    match side {
        EdgeSide::Top | EdgeSide::Bottom if v_boundary_at_row => return None,
        EdgeSide::Left | EdgeSide::Right if h_boundary_at_col => return None,
        _ => {}
    }
    let target = pane_rects
        .iter()
        .find(|(_, r)| {
            let r_right = r.x.saturating_add(r.width);
            let r_bottom = r.y.saturating_add(r.height);
            match side {
                EdgeSide::Top => r.y == pane_area.y && col >= r.x && col < r_right,
                EdgeSide::Bottom => r_bottom == bottom && col >= r.x && col < r_right,
                EdgeSide::Left => r.x == pane_area.x && row >= r.y && row < r_bottom,
                EdgeSide::Right => r_right == right && row >= r.y && row < r_bottom,
            }
        })
        .map(|(id, _)| *id)?;
    Some((side, target))
}

/// Split direction and placement implied by clicking each outer edge.
/// Top/Bottom edges run a horizontal split (rows above / rows below);
/// Left/Right edges run a vertical split (cols left / cols right).
/// Top and Left place the new pane in the first child slot so the
/// visual "the clicked edge spawns the new pane on that side" rule
/// holds.
pub(crate) fn split_intent_for_edge(side: EdgeSide) -> (SplitDirection, bool) {
    match side {
        EdgeSide::Top => (SplitDirection::Horizontal, true),
        EdgeSide::Bottom => (SplitDirection::Horizontal, false),
        EdgeSide::Left => (SplitDirection::Vertical, true),
        EdgeSide::Right => (SplitDirection::Vertical, false),
    }
}

/// Decide how a double-click on the shared internal boundary at
/// `(col, row)` should split. Returns `(direction, target_leaf,
/// new_pane_first)` so the caller can run `split_focused_pane_with_
/// position` against `target_leaf`; the new pane lands right on the
/// clicked divider, between the two siblings it separated.
///
/// Detection is purely geometric (off `pane_rects`), so it works for
/// boundaries at any nesting depth — the leaf whose trailing edge
/// abuts the divider at the clicked row/col is the split target. A
/// vertical divider splits its left leaf to the right; a horizontal
/// divider splits its top leaf downward. Junction cells where a
/// vertical and a horizontal divider cross are ambiguous (which way to
/// split?) and return `None`, mirroring the corner-cell rejection in
/// [`detect_outer_edge`]. Cells on the workspace's outer rim are not
/// shared boundaries and return `None` — those belong to the
/// outer-edge double-click path (#245).
pub(crate) fn detect_shared_boundary(
    pane_area: Rect,
    pane_rects: &[(usize, Rect)],
    col: u16,
    row: u16,
) -> Option<(SplitDirection, usize, bool)> {
    if pane_area.width == 0 || pane_area.height == 0 {
        return None;
    }
    let outer_right = pane_area.x.saturating_add(pane_area.width);
    let outer_bottom = pane_area.y.saturating_add(pane_area.height);

    // A vertical divider sits between a leaf's right border column
    // (`r_right - 1`) and its right neighbor's left border column
    // (`r_right`); the resize hit-test treats both as on the divider.
    // Require `r_right < outer_right` so the workspace's own right rim
    // (an outer edge, not a shared boundary) is excluded.
    let vertical = pane_rects.iter().find_map(|(id, r)| {
        let r_right = r.x.saturating_add(r.width);
        let on_col = col == r_right.saturating_sub(1) || col == r_right;
        let spans_row = row >= r.y && row < r.y.saturating_add(r.height);
        (on_col && spans_row && r_right < outer_right).then_some(*id)
    });
    let horizontal = pane_rects.iter().find_map(|(id, r)| {
        let r_bottom = r.y.saturating_add(r.height);
        let on_row = row == r_bottom.saturating_sub(1) || row == r_bottom;
        let spans_col = col >= r.x && col < r.x.saturating_add(r.width);
        (on_row && spans_col && r_bottom < outer_bottom).then_some(*id)
    });

    match (vertical, horizontal) {
        // Crossing dividers: ambiguous, leave it to the resize-drag.
        (Some(_), Some(_)) => None,
        (Some(id), None) => Some((SplitDirection::Vertical, id, false)),
        (None, Some(id)) => Some((SplitDirection::Horizontal, id, false)),
        (None, None) => None,
    }
}

impl App {
    fn scroll_pane_to_click(&self, pane_id: usize, click_row: u16, inner: &Rect) {
        if let Some(pane) = self.ws().panes.get(&pane_id) {
            let (_, total_lines) = pane.scrollbar_info();
            let visible_rows = inner.height as usize;
            if total_lines <= visible_rows {
                return;
            }
            let max_scroll = total_lines.saturating_sub(visible_rows);
            let relative_y = click_row.saturating_sub(inner.y) as f32;
            let ratio = relative_y / inner.height.max(1) as f32;
            let target_scroll = ((1.0 - ratio) * max_scroll as f32) as usize;
            let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
            parser.screen_mut().set_scrollback(target_scroll);
        }
    }

    /// Bounding box of all pane rects in the active tab — the area the
    /// layout tree was rendered into. `None` when there are no panes
    /// yet (before the first frame).
    fn pane_area(&self) -> Option<Rect> {
        let rects = &self.ws().last_pane_rects;
        if rects.is_empty() {
            return None;
        }
        let min_x = rects.iter().map(|(_, r)| r.x).min().unwrap_or(0);
        let min_y = rects.iter().map(|(_, r)| r.y).min().unwrap_or(0);
        let max_x = rects.iter().map(|(_, r)| r.x + r.width).max().unwrap_or(0);
        let max_y = rects.iter().map(|(_, r)| r.y + r.height).max().unwrap_or(0);
        Some(Rect::new(min_x, min_y, max_x - min_x, max_y - min_y))
    }

    /// If `(col, row)` lands on a shared internal split divider, return
    /// the [`DragTarget::PaneSplit`] that would resize it. Honors each
    /// divider's perpendicular span so a nested divider only claims the
    /// rows/cols it actually occupies (the gap #246 left open). Shared
    /// by the resize-drag setup, the double-click classifier, and the
    /// hover tint so all three agree on what counts as "on a boundary".
    fn boundary_drag_target(&self, col: u16, row: u16) -> Option<DragTarget> {
        let pane_area = self.pane_area()?;
        for b in self.ws().layout.split_boundaries(pane_area) {
            let on_border = match b.direction {
                SplitDirection::Vertical => {
                    col >= b.position.saturating_sub(1)
                        && col <= b.position
                        && row >= b.span.0
                        && row < b.span.1
                }
                SplitDirection::Horizontal => {
                    row >= b.position.saturating_sub(1)
                        && row <= b.position
                        && col >= b.span.0
                        && col < b.span.1
                }
            };
            if on_border {
                // Store the split node's own rect (not `pane_area`) so
                // the resize-drag ratio is measured against the region
                // the divider actually slices — nested dividers would
                // otherwise jump when dragged.
                return Some(DragTarget::PaneSplit(b.path, b.direction, b.area));
            }
        }
        None
    }

    /// Column the file tree starts at, derived from the panel to its
    /// left rather than from the tree's own cached rect so it stays
    /// answerable while the tree is not being painted (mid-drag through
    /// the degrade ladder, or `replace` mode).
    fn file_tree_origin_x(&self) -> u16 {
        self.last_org_sidebar_rect
            .map(|r| r.x + r.width)
            .unwrap_or(0)
    }

    fn is_on_org_sidebar_border(&self, col: u16) -> bool {
        if let Some(rect) = self.last_org_sidebar_rect {
            let border_col = rect.x + rect.width;
            col >= border_col.saturating_sub(1) && col <= border_col
        } else {
            false
        }
    }

    fn is_on_file_tree_border(&self, col: u16) -> bool {
        if let Some(rect) = self.ws().last_file_tree_rect {
            let border_col = rect.x + rect.width;
            col >= border_col.saturating_sub(1) && col <= border_col
        } else {
            false
        }
    }

    fn is_on_preview_border(&self, col: u16) -> bool {
        if let Some(rect) = self.ws().last_preview_rect {
            let border_col = if self.layout_swapped {
                rect.x + rect.width
            } else {
                rect.x
            };
            col >= border_col.saturating_sub(1) && col <= border_col
        } else {
            false
        }
    }

    /// End an in-flight drag without acting on it: balance a
    /// forwarded press with a release so the pane's mouse-reporting
    /// app doesn't stay stuck mid-drag, then drop the drag target.
    /// Used by the close-confirm swallow path, which consumes every
    /// pointer event but cannot afford to lose the release.
    fn release_in_flight_drag(&mut self, mouse: &MouseEvent) {
        if let Some(DragTarget::PaneMouseReport(ws_idx, pane_id, rect, btn)) = self.dragging.clone()
        {
            self.forward_pointer_to_pane(ws_idx, pane_id, rect, btn, PointerAction::Release, mouse);
        }
        self.dragging = None;
    }

    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        // Close confirmation swallows the pointer (Issue #285). Down /
        // drag / scroll would otherwise be forwarded to a
        // mouse-reporting TUI as control sequences, or move focus out
        // from under the pinned target — neither is acceptable while a
        // modal is asking a yes/no question.
        //
        // A *release* is the one event that still has to be observed.
        // `self.dragging` is only ever cleared on `Up`, and a
        // `PaneMouseReport` drag owes its PTY the release balancing the
        // press already forwarded to it. Dropping that event would
        // strand a drag target to hijack the next unrelated gesture and
        // leave the pane's TUI stuck mid-drag.
        if self.close_confirm.is_some() {
            if matches!(mouse.kind, MouseEventKind::Up(_)) {
                self.release_in_flight_drag(&mouse);
            }
            return;
        }

        if matches!(mouse.kind, MouseEventKind::Down(_)) && self.rename_input.is_some() {
            let needs_relayout = !self.status_bar_visible;
            self.rename_input = None;
            self.dirty = true;
            if needs_relayout {
                self.mark_layout_change();
            }
        }

        if let Some(DragTarget::PaneMouseReport(ws_idx, pane_id, rect, btn)) = self.dragging.clone()
        {
            match mouse.kind {
                MouseEventKind::Drag(_) => {
                    self.forward_pointer_to_pane(
                        ws_idx,
                        pane_id,
                        rect,
                        btn,
                        PointerAction::Drag,
                        &mouse,
                    );
                    return;
                }
                MouseEventKind::Up(_) => {
                    self.forward_pointer_to_pane(
                        ws_idx,
                        pane_id,
                        rect,
                        btn,
                        PointerAction::Release,
                        &mouse,
                    );
                    self.dragging = None;
                    return;
                }
                _ => {}
            }
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;
                self.selection = None;

                for &(tab_idx, rect) in &self.last_tab_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        let now = Instant::now();
                        let is_double = matches!(
                            self.last_tab_click,
                            Some((prev_idx, prev_t))
                                if prev_idx == tab_idx
                                    && now.duration_since(prev_t).as_millis() < 500
                        );
                        // `switch_tab` clears `last_tab_click` along with
                        // the other tab-keyed caches, so the double-click
                        // verdict is read above and re-armed below.
                        self.switch_tab(tab_idx);
                        if is_double {
                            self.rename_input = Some(String::new());
                            self.last_tab_click = None;
                        } else {
                            self.last_tab_click = Some((tab_idx, now));
                        }
                        self.last_edge_click = None;
                        self.last_boundary_click = None;
                        self.dirty = true;
                        return;
                    }
                }
                self.last_tab_click = None;

                if let Some(rect) = self.last_new_tab_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        if let Ok(new_id) = self.new_tab() {
                            self.emit_pane_started(new_id);
                        }
                        self.last_edge_click = None;
                        self.last_boundary_click = None;
                        return;
                    }
                }

                // Checked before the file tree because in `coexist` the
                // sidebar sits immediately to its left, so the two
                // 2-column hit areas can abut. Outermost panel wins.
                if self.is_on_org_sidebar_border(col) {
                    self.dragging = Some(DragTarget::OrgSidebarBorder);
                    self.last_edge_click = None;
                    self.last_boundary_click = None;
                    return;
                }
                if self.is_on_file_tree_border(col) {
                    self.dragging = Some(DragTarget::FileTreeBorder);
                    self.last_edge_click = None;
                    self.last_boundary_click = None;
                    return;
                }
                if self.is_on_preview_border(col) {
                    self.dragging = Some(DragTarget::PreviewBorder);
                    self.last_edge_click = None;
                    self.last_boundary_click = None;
                    return;
                }

                if let Some(pane_area) = self.pane_area() {
                    let active_tab = self.active_tab;

                    // Shared internal boundary: classify click vs drag.
                    // A second click on the same divider cell within
                    // 500 ms double-clicks it → split the adjacent pane
                    // and drop the new leaf right on the divider. A
                    // first (single) click instead arms the timer and
                    // sets up the resize-drag, so a plain click that
                    // never moves is a no-op and a click-drag still
                    // resizes — the historical behavior. (Issue #247)
                    if let Some(DragTarget::PaneSplit(path, direction, area)) =
                        self.boundary_drag_target(col, row)
                    {
                        let now = Instant::now();
                        let is_double = matches!(
                            self.last_boundary_click,
                            Some((prev_tab, prev_col, prev_row, prev_t))
                                if prev_tab == active_tab
                                    && prev_col == col
                                    && prev_row == row
                                    && now.duration_since(prev_t).as_millis() < 500
                        );
                        // A boundary click breaks any in-flight
                        // outer-edge double-click intent.
                        self.last_edge_click = None;
                        if is_double {
                            self.last_boundary_click = None;
                            let pane_rects = self.ws().last_pane_rects.clone();
                            if let Some((dir, target_id, new_pane_first)) =
                                detect_shared_boundary(pane_area, &pane_rects, col, row)
                            {
                                self.ws_mut().focused_pane_id = target_id;
                                self.ws_mut().focus_target = FocusTarget::Pane;
                                if let Ok(Some(new_id)) =
                                    self.split_focused_pane_with_position(dir, new_pane_first, None)
                                {
                                    self.emit_pane_started(new_id);
                                }
                                return;
                            }
                            // Ambiguous junction cell — fall through to
                            // the resize-drag instead of splitting.
                        } else {
                            self.last_boundary_click = Some((active_tab, col, row, now));
                        }
                        self.dragging = Some(DragTarget::PaneSplit(path, direction, area));
                        return;
                    }

                    // Outer-edge double-click → split. The shared-boundary
                    // resize check above already returned for any internal
                    // border, so reaching here means we're either on an
                    // outer edge or off the layout entirely. We record
                    // single clicks so the second within 500ms triggers a
                    // split on the same edge cell; control flow then falls
                    // through to the per-pane focus loop so a single click
                    // on a pane's outer border still focuses that pane,
                    // preserving the historical behavior. (Issue #245)
                    let pane_rects = self.ws().last_pane_rects.clone();
                    if let Some((side, target_id)) =
                        detect_outer_edge(pane_area, &pane_rects, col, row)
                    {
                        let now = Instant::now();
                        let is_double = matches!(
                            self.last_edge_click,
                            Some((prev_tab, prev_col, prev_row, prev_t))
                                if prev_tab == active_tab
                                    && prev_col == col
                                    && prev_row == row
                                    && now.duration_since(prev_t).as_millis() < 500
                        );
                        if is_double {
                            self.last_edge_click = None;
                            self.ws_mut().focused_pane_id = target_id;
                            self.ws_mut().focus_target = FocusTarget::Pane;
                            let (direction, new_pane_first) = split_intent_for_edge(side);
                            if let Ok(Some(new_id)) = self.split_focused_pane_with_position(
                                direction,
                                new_pane_first,
                                None,
                            ) {
                                self.emit_pane_started(new_id);
                            }
                            return;
                        } else {
                            self.last_edge_click = Some((active_tab, col, row, now));
                            // Don't return — let the per-pane focus loop
                            // run so a single edge click still selects
                            // the underlying pane.
                        }
                        // An outer-edge click is never a boundary click;
                        // reset the boundary timer so the two paths
                        // can't cross-trigger.
                        self.last_boundary_click = None;
                    } else {
                        self.last_edge_click = None;
                        self.last_boundary_click = None;
                    }
                }

                if let Some(rect) = self.last_org_sidebar_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().focus_target = FocusTarget::OrgSidebar;
                        // The rect includes the block's border rows, and
                        // `org_sidebar_row_targets` holds *every* row,
                        // not just the visible window. Clip to the inner
                        // area explicitly: without it a click on the
                        // " ORG " title row saturated to inner row 0 and
                        // activated whatever was at the top of the view,
                        // and a click on the bottom border resolved one
                        // row *past* the window and jumped to a tab that
                        // was never on screen. Activation switches tabs
                        // and moves pane focus, so a stray hit is not a
                        // cosmetic slip.
                        let visible_height = rect.height.saturating_sub(2) as usize;
                        let on_inner_row = row > rect.y && row + 1 < rect.y + rect.height;
                        let inner_y = row.saturating_sub(rect.y + 1) as usize;
                        if on_inner_row && inner_y < visible_height {
                            let idx = self.org_sidebar_scroll + inner_y;
                            // `switch_tab` re-validates the tab index
                            // against `workspaces` regardless, since an
                            // MCP close can land between paint and click.
                            if let Some(&target) = self.org_sidebar_row_targets.get(idx) {
                                self.org_sidebar_activate_target(target);
                            }
                        }
                        self.last_edge_click = None;
                        self.last_boundary_click = None;
                        self.dirty = true;
                        return;
                    }
                }

                if let Some(rect) = self.ws().last_file_tree_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().focus_target = FocusTarget::FileTree;
                        let inner_y = row.saturating_sub(rect.y + 1);
                        let scroll = self.ws().file_tree.scroll_offset;
                        let entry_idx = scroll + inner_y as usize;
                        let entry_count = self.ws().file_tree.visible_entries().len();
                        if entry_idx < entry_count {
                            self.ws_mut().file_tree.selected_index = entry_idx;
                            let path = self.ws_mut().file_tree.toggle_or_select();
                            if let Some(path) = path {
                                self.clear_selection_if_preview();
                                let messages = self.messages();
                                let mut picker = self.image_picker.take();
                                self.ws_mut().preview.load(&path, picker.as_mut(), messages);
                                self.image_picker = picker;
                            }
                        }
                        self.last_edge_click = None;
                        self.last_boundary_click = None;
                        return;
                    }
                }

                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().focus_target = FocusTarget::Preview;
                        self.last_edge_click = None;
                        self.last_boundary_click = None;
                        return;
                    }
                }

                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().focused_pane_id = pane_id;
                        self.ws_mut().focus_target = FocusTarget::Pane;

                        if !mouse.modifiers.contains(KeyModifiers::SHIFT)
                            && !mouse_forward_disabled()
                            && self.try_forward_pane_press(
                                pane_id,
                                rect,
                                PointerButton::Left,
                                col,
                                row,
                            )
                        {
                            return;
                        }

                        let scrollbar_col = rect.x + rect.width - 2;
                        if col >= scrollbar_col {
                            let inner = Rect::new(
                                rect.x + 1,
                                rect.y + 1,
                                rect.width.saturating_sub(2),
                                rect.height.saturating_sub(2),
                            );
                            self.scroll_pane_to_click(pane_id, row, &inner);
                            self.dragging = Some(DragTarget::Scrollbar(pane_id, inner));
                        }
                        return;
                    }
                }
            }
            MouseEventKind::Down(btn @ (MouseButton::Middle | MouseButton::Right)) => {
                let col = mouse.column;
                let row = mouse.row;
                if mouse.modifiers.contains(KeyModifiers::SHIFT) || mouse_forward_disabled() {
                    return;
                }
                let pointer_btn = match btn {
                    MouseButton::Middle => PointerButton::Middle,
                    MouseButton::Right => PointerButton::Right,
                    MouseButton::Left => unreachable!(),
                };
                let pane_rects = self.ws().last_pane_rects.clone();
                for (pane_id, rect) in pane_rects {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.try_forward_pane_press(pane_id, rect, pointer_btn, col, row);
                        return;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;

                if let Some(ref target) = self.dragging.clone() {
                    match target {
                        DragTarget::FileTreeBorder => {
                            // Was `col.clamp(10, 60)`, which silently
                            // assumed the tree starts at column 0. With
                            // the org sidebar to its left that is no
                            // longer true.
                            //
                            // Derived from the sidebar's width rather
                            // than read off `last_file_tree_rect`: a
                            // drag wide enough to trip the degrade
                            // ladder makes the tree's own rect `None`
                            // mid-drag, and gating on it would freeze
                            // the width there — dragging back left
                            // could never restore the tree, and the
                            // border would no longer hit-test, so there
                            // was no way out short of resizing the
                            // terminal.
                            self.file_tree_width =
                                col.saturating_sub(self.file_tree_origin_x()).clamp(10, 60);
                        }
                        DragTarget::OrgSidebarBorder => {
                            // The sidebar is always the outermost panel,
                            // so its left edge is the main area's x — no
                            // cached rect needed, and the same mid-drag
                            // recovery argument as above applies.
                            self.org_sidebar_width = col.clamp(
                                crate::app::layout_geometry::ORG_SIDEBAR_MIN_WIDTH,
                                crate::app::layout_geometry::ORG_SIDEBAR_MAX_WIDTH,
                            );
                        }
                        DragTarget::PreviewBorder => {
                            if let Some(rect) = self.ws().last_preview_rect {
                                if self.layout_swapped {
                                    let new_width = col.saturating_sub(rect.x).clamp(15, 80);
                                    self.preview_width = new_width;
                                } else {
                                    let total_right = rect.x + rect.width;
                                    let new_width = total_right.saturating_sub(col).clamp(15, 80);
                                    self.preview_width = new_width;
                                }
                            }
                        }
                        DragTarget::PaneSplit(path, direction, area) => {
                            let new_ratio = match direction {
                                SplitDirection::Vertical => {
                                    (col.saturating_sub(area.x) as f32) / area.width.max(1) as f32
                                }
                                SplitDirection::Horizontal => {
                                    (row.saturating_sub(area.y) as f32) / area.height.max(1) as f32
                                }
                            };
                            self.ws_mut().layout.update_ratio(path, new_ratio);
                            // An actual resize-drag invalidates the
                            // pending double-click: the next click on
                            // this cell should arm a fresh timer, not
                            // promote a just-resized divider into a
                            // phantom double-click split.
                            self.last_boundary_click = None;
                        }
                        DragTarget::Scrollbar(pane_id, inner) => {
                            self.scroll_pane_to_click(*pane_id, row, inner);
                        }
                        DragTarget::PaneMouseReport(..) => {
                            debug_assert!(false, "PaneMouseReport leaked to border-drag match");
                        }
                    }
                    return;
                }

                if let Some(ref mut sel) = self.selection {
                    let inner = sel.content_rect;
                    match sel.target {
                        SelectionTarget::Pane(_) => {
                            sel.end_col = col
                                .saturating_sub(inner.x)
                                .min(inner.width.saturating_sub(1))
                                as u32;
                            sel.end_row = row
                                .saturating_sub(inner.y)
                                .min(inner.height.saturating_sub(1))
                                as u32;
                        }
                        SelectionTarget::Preview => {
                            let scroll_v = self.ws().preview.scroll_offset;
                            let h_scroll = self.ws().preview.h_scroll_offset;

                            let mut screen_col = col.saturating_sub(inner.x);
                            let mut screen_row = row.saturating_sub(inner.y);

                            if col < inner.x {
                                self.ws_mut().preview.scroll_left(2);
                                screen_col = 0;
                            } else if col >= inner.x + inner.width {
                                self.ws_mut().preview.scroll_right(2);
                                screen_col = inner.width.saturating_sub(1);
                            }
                            if row < inner.y {
                                self.ws_mut().preview.scroll_up(1);
                                screen_row = 0;
                            } else if row >= inner.y + inner.height {
                                self.ws_mut().preview.scroll_down(1);
                                screen_row = inner.height.saturating_sub(1);
                            }

                            let scroll_v = self.ws().preview.scroll_offset.max(scroll_v);
                            let h_scroll = self.ws().preview.h_scroll_offset.max(h_scroll);
                            let lines_len = self.ws().preview.lines.len();
                            let abs_row =
                                (scroll_v + screen_row as usize).min(lines_len.saturating_sub(1));
                            let abs_col = screen_col as usize + h_scroll;
                            if let Some(sel) = self.selection.as_mut() {
                                sel.end_row = abs_row as u32;
                                sel.end_col = abs_col as u32;
                            }
                        }
                    }
                } else {
                    let pane_rects = self.ws().last_pane_rects.clone();
                    let mut started = false;
                    for (pane_id, rect) in pane_rects {
                        if col >= rect.x
                            && col < rect.x + rect.width
                            && row >= rect.y
                            && row < rect.y + rect.height
                        {
                            let inner = Rect::new(
                                rect.x + 1,
                                rect.y + 1,
                                rect.width.saturating_sub(2),
                                rect.height.saturating_sub(2),
                            );
                            let cell_col = col.saturating_sub(inner.x) as u32;
                            let cell_row = row.saturating_sub(inner.y) as u32;
                            self.selection = Some(TextSelection {
                                target: SelectionTarget::Pane(pane_id),
                                start_row: cell_row,
                                start_col: cell_col,
                                end_row: cell_row,
                                end_col: cell_col,
                                content_rect: inner,
                            });
                            started = true;
                            break;
                        }
                    }
                    if !started {
                        if let Some(rect) = self.ws().last_preview_rect {
                            if col >= rect.x
                                && col < rect.x + rect.width
                                && row >= rect.y
                                && row < rect.y + rect.height
                            {
                                const GUTTER: u16 = 5;
                                let inner = Rect::new(
                                    rect.x + 1 + GUTTER,
                                    rect.y + 1,
                                    rect.width.saturating_sub(2 + GUTTER),
                                    rect.height.saturating_sub(2),
                                );
                                if col >= inner.x && row >= inner.y {
                                    let screen_col = col.saturating_sub(inner.x);
                                    let screen_row = row.saturating_sub(inner.y);
                                    let scroll_v = self.ws().preview.scroll_offset;
                                    let h_scroll = self.ws().preview.h_scroll_offset;
                                    let lines_len = self.ws().preview.lines.len();
                                    let abs_row = (scroll_v + screen_row as usize)
                                        .min(lines_len.saturating_sub(1));
                                    let abs_col = screen_col as usize + h_scroll;
                                    self.selection = Some(TextSelection {
                                        target: SelectionTarget::Preview,
                                        start_row: abs_row as u32,
                                        start_col: abs_col as u32,
                                        end_row: abs_row as u32,
                                        end_col: abs_col as u32,
                                        content_rect: inner,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = None;

                if let Some(sel) = self.selection.clone() {
                    let (sr, sc, er, ec) = sel.normalized();
                    if sr != er || sc != ec {
                        let text = match sel.target {
                            SelectionTarget::Pane(pane_id) => self
                                .ws()
                                .panes
                                .get(&pane_id)
                                .map(|p| extract_selected_text(p, sr, sc, er, ec))
                                .unwrap_or_default(),
                            SelectionTarget::Preview => {
                                extract_preview_selected_text(&self.ws().preview, sr, sc, er, ec)
                            }
                        };
                        if !text.is_empty() {
                            self.copy_to_clipboard(&text);
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => self.handle_wheel(mouse.column, mouse.row, false),
            MouseEventKind::ScrollDown => self.handle_wheel(mouse.column, mouse.row, true),
            MouseEventKind::ScrollLeft => {
                let col = mouse.column;
                let row = mouse.row;
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_left(4);
                    }
                }
            }
            MouseEventKind::ScrollRight => {
                let col = mouse.column;
                let row = mouse.row;
                if let Some(rect) = self.ws().last_preview_rect {
                    if col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height
                    {
                        self.ws_mut().preview.scroll_right(4);
                    }
                }
            }
            MouseEventKind::Moved => {
                let col = mouse.column;
                let row = mouse.row;
                let old_hover = self.hover_border.clone();
                if self.is_on_org_sidebar_border(col) {
                    self.hover_border = Some(DragTarget::OrgSidebarBorder);
                } else if self.is_on_file_tree_border(col) {
                    self.hover_border = Some(DragTarget::FileTreeBorder);
                } else if self.is_on_preview_border(col) {
                    self.hover_border = Some(DragTarget::PreviewBorder);
                } else {
                    // Tint the shared internal divider under the cursor
                    // so it reads as draggable / double-clickable, the
                    // same affordance the file-tree and preview borders
                    // already get. (Issue #247)
                    self.hover_border = self.boundary_drag_target(col, row);
                }
                if self.hover_border != old_hover {
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn handle_wheel(&mut self, col: u16, row: u16, scroll_down: bool) {
        if let Some(rect) = self.last_org_sidebar_rect {
            if col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
            {
                // Scrolls the view only; the selection stays put. The
                // renderer re-anchors on the selection only when
                // `org_sidebar_follow_selection` is set (i.e. after a
                // j/k/Enter), which is what keeps this from being
                // undone on the very next frame.
                let rows = self.org_sidebar_row_targets.len();
                let visible = rect.height.saturating_sub(2) as usize;
                let max_scroll = rows.saturating_sub(visible);
                self.org_sidebar_scroll = if scroll_down {
                    (self.org_sidebar_scroll + 3).min(max_scroll)
                } else {
                    self.org_sidebar_scroll.saturating_sub(3)
                };
                self.dirty = true;
                return;
            }
        }
        if let Some(rect) = self.ws().last_file_tree_rect {
            if col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
            {
                if scroll_down {
                    self.ws_mut().file_tree.scroll_down(3);
                } else {
                    self.ws_mut().file_tree.scroll_up(3);
                }
                return;
            }
        }
        if let Some(rect) = self.ws().last_preview_rect {
            if col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
            {
                if scroll_down {
                    self.ws_mut().preview.scroll_down(3);
                } else {
                    self.ws_mut().preview.scroll_up(3);
                }
                return;
            }
        }

        let disable_forward = mouse_forward_disabled();
        let pane_rects = self.ws().last_pane_rects.clone();
        for (pane_id, rect) in pane_rects {
            if !(col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height)
            {
                continue;
            }
            let local_col = col.saturating_sub(rect.x).saturating_sub(1);
            let local_row = row.saturating_sub(rect.y).saturating_sub(1);
            let codex_hint = self.pane_expects_codex_peer_delivery(self.active_tab, pane_id);

            let bytes = if disable_forward {
                None
            } else {
                self.ws().panes.get(&pane_id).and_then(|p| {
                    p.wheel_forward_bytes(codex_hint, scroll_down, local_col, local_row)
                })
            };

            if let Some(data) = bytes {
                if let Some(pane) = self.ws_mut().panes.get_mut(&pane_id) {
                    let _ = pane.write_input(&data);
                    self.dirty = true;
                }
            } else if let Some(pane) = self.ws().panes.get(&pane_id) {
                if scroll_down {
                    pane.scroll_down(3);
                } else {
                    pane.scroll_up(3);
                }
                self.dirty = true;
            }
            return;
        }
    }

    fn try_forward_pane_press(
        &mut self,
        pane_id: usize,
        rect: Rect,
        button: PointerButton,
        col: u16,
        row: u16,
    ) -> bool {
        let (local_col, local_row) = match pane_local_coords(rect, col, row) {
            Some(lc) => lc,
            None => return false,
        };
        let bytes = self.ws().panes.get(&pane_id).and_then(|p| {
            p.click_forward_bytes(
                self.pane_expects_codex_peer_delivery(self.active_tab, pane_id),
                button,
                PointerAction::Press,
                local_col,
                local_row,
            )
        });
        let Some(data) = bytes else {
            return false;
        };
        let ws_idx = self.active_tab;
        if let Some(pane) = self.ws_mut().panes.get_mut(&pane_id) {
            let _ = pane.write_input(&data);
            self.dragging = Some(DragTarget::PaneMouseReport(ws_idx, pane_id, rect, button));
            self.dirty = true;
            return true;
        }
        false
    }

    fn forward_pointer_to_pane(
        &mut self,
        ws_idx: usize,
        pane_id: usize,
        rect: Rect,
        button: PointerButton,
        action: PointerAction,
        mouse: &MouseEvent,
    ) {
        let (local_col, local_row) = pane_local_coords_clamped(rect, mouse.column, mouse.row);
        let bytes = self
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.panes.get(&pane_id))
            .and_then(|p| {
                p.click_forward_bytes(
                    self.pane_expects_codex_peer_delivery(ws_idx, pane_id),
                    button,
                    action,
                    local_col,
                    local_row,
                )
            });
        let Some(data) = bytes else {
            return;
        };
        if let Some(pane) = self
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.panes.get_mut(&pane_id))
        {
            let _ = pane.write_input(&data);
            if ws_idx == self.active_tab {
                self.dirty = true;
            }
        }
    }
}

pub(crate) fn mouse_forward_disabled() -> bool {
    std::env::var("RENGA_DISABLE_MOUSE_FORWARD")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

pub(crate) fn pane_local_coords(rect: Rect, col: u16, row: u16) -> Option<(u16, u16)> {
    if rect.width < 3 || rect.height < 3 {
        return None;
    }
    let right = rect.x.saturating_add(rect.width);
    let bottom = rect.y.saturating_add(rect.height);
    if col <= rect.x || col.saturating_add(1) >= right {
        return None;
    }
    if row <= rect.y || row.saturating_add(1) >= bottom {
        return None;
    }
    Some((col - rect.x - 1, row - rect.y - 1))
}

pub(crate) fn pane_local_coords_clamped(rect: Rect, col: u16, row: u16) -> (u16, u16) {
    let inner_x = rect.x.saturating_add(1);
    let inner_y = rect.y.saturating_add(1);
    let inner_w = rect.width.saturating_sub(2);
    let inner_h = rect.height.saturating_sub(2);
    let max_col = inner_w.saturating_sub(1);
    let max_row = inner_h.saturating_sub(1);
    let local_col = col.saturating_sub(inner_x).min(max_col);
    let local_row = row.saturating_sub(inner_y).min(max_row);
    (local_col, local_row)
}
