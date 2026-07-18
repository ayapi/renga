use super::*;

/// Split direction for layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SplitDirection {
    Vertical,
    Horizontal,
}

/// One internal split divider, used for resize / hover hit-testing.
/// `position` is the screen column (for a `Vertical` split) or row (for
/// a `Horizontal` split) the divider sits on; `path` identifies the
/// owning `Split` node for [`LayoutNode::update_ratio`]. `span` is the
/// divider's perpendicular extent — `(start, end)` half-open rows for a
/// `Vertical` split, columns for a `Horizontal` one. Before #247 the
/// hit-test assumed every divider spanned the whole pane area, so a
/// nested divider (e.g. a left/right split living only in the bottom
/// half) wrongly claimed clicks that shared its column but fell in the
/// unrelated top pane. Carrying `span` closes that gap.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitBoundary {
    pub position: u16,
    pub direction: SplitDirection,
    pub path: Vec<bool>,
    pub span: (u16, u16),
    /// The owning `Split` node's own rendered rect. Resize-drag ratios
    /// must be measured against this sub-region, not the whole pane
    /// area — otherwise a divider nested inside one half of the layout
    /// jumps when dragged (the ratio would be taken relative to the
    /// full width/height instead of the node's slice).
    pub area: Rect,
}

/// Binary tree node for pane layout.
#[derive(Debug)]
pub enum LayoutNode {
    Leaf {
        pane_id: usize,
    },
    Split {
        direction: SplitDirection,
        ratio: f32, // 0.0..1.0, portion allocated to first child
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn collect_pane_ids(&self) -> Vec<usize> {
        match self {
            LayoutNode::Leaf { pane_id } => vec![*pane_id],
            LayoutNode::Split { first, second, .. } => {
                let mut ids = first.collect_pane_ids();
                ids.extend(second.collect_pane_ids());
                ids
            }
        }
    }

    pub fn calculate_rects(&self, area: Rect) -> Vec<(usize, Rect)> {
        match self {
            LayoutNode::Leaf { pane_id } => vec![(*pane_id, area)],
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (first_area, second_area) = split_rect(area, *direction, *ratio);
                let mut result = first.calculate_rects(first_area);
                result.extend(second.calculate_rects(second_area));
                result
            }
        }
    }

    /// Split the leaf with id `target_id`, inserting a new leaf
    /// `new_id`. `new_pane_first` decides whether the new pane lands in
    /// the first (top/left) or second (bottom/right) child slot.
    /// `Alt+D` / `Alt+E` go through `split_focused_pane` with
    /// `new_pane_first = false` to preserve the historical "new pane
    /// on the trailing side" placement; outer-edge double-clicks on
    /// the top/left edges pass `true` so the new pane appears on the
    /// clicked side.
    pub fn split_pane_with_position(
        &mut self,
        target_id: usize,
        new_id: usize,
        direction: SplitDirection,
        new_pane_first: bool,
    ) -> bool {
        match self {
            LayoutNode::Leaf { pane_id } => {
                if *pane_id == target_id {
                    let old_id = *pane_id;
                    let (first_id, second_id) = if new_pane_first {
                        (new_id, old_id)
                    } else {
                        (old_id, new_id)
                    };
                    *self = LayoutNode::Split {
                        direction,
                        ratio: 0.5,
                        first: Box::new(LayoutNode::Leaf { pane_id: first_id }),
                        second: Box::new(LayoutNode::Leaf { pane_id: second_id }),
                    };
                    true
                } else {
                    false
                }
            }
            LayoutNode::Split { first, second, .. } => {
                first.split_pane_with_position(target_id, new_id, direction, new_pane_first)
                    || second.split_pane_with_position(target_id, new_id, direction, new_pane_first)
            }
        }
    }

    pub fn remove_pane(&mut self, target_id: usize) -> bool {
        match self {
            LayoutNode::Leaf { .. } => false,
            LayoutNode::Split { first, second, .. } => {
                if let LayoutNode::Leaf { pane_id } = first.as_ref() {
                    if *pane_id == target_id {
                        let second =
                            std::mem::replace(second.as_mut(), LayoutNode::Leaf { pane_id: 0 });
                        *self = second;
                        return true;
                    }
                }
                if let LayoutNode::Leaf { pane_id } = second.as_ref() {
                    if *pane_id == target_id {
                        let first =
                            std::mem::replace(first.as_mut(), LayoutNode::Leaf { pane_id: 0 });
                        *self = first;
                        return true;
                    }
                }
                first.remove_pane(target_id) || second.remove_pane(target_id)
            }
        }
    }

    /// Find every internal split divider for hit testing. Each
    /// [`SplitBoundary`] carries its screen position, direction, tree
    /// path, and perpendicular span (the rows/cols it actually
    /// occupies, so nested dividers don't over-claim — see #247).
    pub fn split_boundaries(&self, area: Rect) -> Vec<SplitBoundary> {
        let mut result = Vec::new();
        self.collect_boundaries(area, &mut Vec::new(), &mut result);
        result
    }

    fn collect_boundaries(
        &self,
        area: Rect,
        path: &mut Vec<bool>, // false=first, true=second
        result: &mut Vec<SplitBoundary>,
    ) {
        if let LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } = self
        {
            let (first_area, second_area) = split_rect(area, *direction, *ratio);

            // The boundary is at the edge between first and second
            let boundary = match direction {
                SplitDirection::Vertical => first_area.x + first_area.width,
                SplitDirection::Horizontal => first_area.y + first_area.height,
            };
            // The divider runs perpendicular to the split: a vertical
            // split's divider spans this node's rows, a horizontal
            // split's spans its columns.
            let span = match direction {
                SplitDirection::Vertical => (area.y, area.y.saturating_add(area.height)),
                SplitDirection::Horizontal => (area.x, area.x.saturating_add(area.width)),
            };
            result.push(SplitBoundary {
                position: boundary,
                direction: *direction,
                path: path.clone(),
                span,
                area,
            });

            path.push(false);
            first.collect_boundaries(first_area, path, result);
            path.pop();

            path.push(true);
            second.collect_boundaries(second_area, path, result);
            path.pop();
        }
    }

    /// Update ratio by path (path identifies which Split node).
    pub fn update_ratio(&mut self, path: &[bool], new_ratio: f32) {
        if path.is_empty() {
            if let LayoutNode::Split { ratio, .. } = self {
                *ratio = new_ratio.clamp(0.15, 0.85);
            }
        } else if let LayoutNode::Split { first, second, .. } = self {
            if path[0] {
                second.update_ratio(&path[1..], new_ratio);
            } else {
                first.update_ratio(&path[1..], new_ratio);
            }
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            LayoutNode::Leaf { .. } => 1,
            LayoutNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }
}

fn split_rect(area: Rect, direction: SplitDirection, ratio: f32) -> (Rect, Rect) {
    let ratio = ratio.clamp(0.1, 0.9);
    match direction {
        SplitDirection::Vertical => {
            let first_w = (area.width as f32 * ratio) as u16;
            let first_w = first_w.max(1).min(area.width.saturating_sub(1));
            (
                Rect::new(area.x, area.y, first_w, area.height),
                Rect::new(area.x + first_w, area.y, area.width - first_w, area.height),
            )
        }
        SplitDirection::Horizontal => {
            let first_h = (area.height as f32 * ratio) as u16;
            let first_h = first_h.max(1).min(area.height.saturating_sub(1));
            (
                Rect::new(area.x, area.y, area.width, first_h),
                Rect::new(area.x, area.y + first_h, area.width, area.height - first_h),
            )
        }
    }
}
