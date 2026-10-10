//! Horizontal geometry for the main area — the single source of truth
//! shared by the renderer and the PTY-resize path.
//!
//! Before the org sidebar landed, `ui::render_main_area` and
//! `App::relayout_panes` each carried their own copy of the same width
//! math: which side panels survive a narrow terminal, in what order the
//! surviving panels sit on screen, and where the pane area therefore
//! starts. `relayout_panes` even said so in a comment ("Mirror the area
//! math in ui::render / render_main_area"). Two hand-synced copies were
//! already a hazard — the renderer paints one geometry while the PTYs
//! are told about another — and adding a third panel with a four-step
//! degrade ladder would have made drift near-certain. Both callers now
//! go through [`compute`].
//!
//! The function is deliberately pure (`Rect` in, `Rect`s out, no `App`)
//! so the degrade ladder can be unit-tested without spawning PTYs.

use ratatui::layout::Rect;

use crate::config::OrgSidebarMode;

/// Narrowest pane area we will squeeze the terminal grid into before
/// dropping side panels. Panes below this stop being usable at all.
pub(crate) const MIN_PANE_AREA_WIDTH: u16 = 20;

/// Width the org sidebar falls back to when the terminal can no longer
/// afford its full width. Wide enough to still show a tab index, a
/// truncated label and the working/idle marker.
pub(crate) const ORG_SIDEBAR_COMPACT_WIDTH: u16 = 16;

/// Drag-resize bounds for the org sidebar, mirroring the file tree's
/// `10..=60` clamp.
pub(crate) const ORG_SIDEBAR_MIN_WIDTH: u16 = 14;
pub(crate) const ORG_SIDEBAR_MAX_WIDTH: u16 = 60;

/// Startup width. Fits `▸ 1 renga` plus a context meter without
/// crowding the pane area on an 80-column terminal.
pub(crate) const DEFAULT_ORG_SIDEBAR_WIDTH: u16 = 26;

/// Everything [`compute`] needs to know about the current UI state.
/// Passed by value because every field is a scalar the caller already
/// holds; keeping `App` out of the signature is what makes the degrade
/// ladder unit-testable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MainAreaInput {
    /// The full main area (below the tab bar, above the status bar).
    pub area: Rect,
    pub org_sidebar_mode: OrgSidebarMode,
    /// Runtime toggle state. Ignored when the mode is `Off`.
    pub org_sidebar_visible: bool,
    pub org_sidebar_width: u16,
    pub file_tree_visible: bool,
    pub file_tree_width: u16,
    pub preview_active: bool,
    pub preview_width: u16,
    pub layout_swapped: bool,
}

/// Resolved on-screen geometry. `None` means the panel is not painted
/// this frame — either because it is toggled off or because the degrade
/// ladder dropped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MainAreaLayout {
    pub org_sidebar: Option<Rect>,
    pub file_tree: Option<Rect>,
    pub preview: Option<Rect>,
    pub panes: Rect,
    /// True when the sidebar survived only by shrinking to
    /// [`ORG_SIDEBAR_COMPACT_WIDTH`]. The renderer uses this to switch
    /// to the abbreviated row format instead of truncating mid-label.
    pub org_sidebar_compact: bool,
}

/// Resolve the horizontal split of the main area.
///
/// Panel order on screen is
/// `[org sidebar] [file tree] [preview if swapped] [panes] [preview if
/// not swapped]`. The org sidebar sits outermost because it is the only
/// cross-tab panel: keeping it pinned to the edge means its rows do not
/// move when the per-tab file tree is toggled.
///
/// Degrade ladder when the terminal is too narrow, in order:
///
/// 1. drop the preview,
/// 2. drop the file tree,
/// 3. shrink the org sidebar to [`ORG_SIDEBAR_COMPACT_WIDTH`],
/// 4. drop the org sidebar.
///
/// Step 4 exists because "always present" is a soft requirement: a pane
/// area below [`MIN_PANE_AREA_WIDTH`] is unusable, and an unusable
/// terminal is worse than a missing sidebar.
///
/// With the sidebar off the ladder collapses to exactly the two steps
/// the app had before (preview, then file tree), so existing narrow-
/// terminal behaviour is unchanged.
pub(crate) fn compute(input: MainAreaInput) -> MainAreaLayout {
    let width = input.area.width;

    let mut show_org = input.org_sidebar_visible && input.org_sidebar_mode != OrgSidebarMode::Off;
    // In `replace` mode the sidebar takes over the file tree's slot
    // rather than sitting next to it, so the two are never both up.
    let mut show_tree =
        input.file_tree_visible && !(input.org_sidebar_mode == OrgSidebarMode::Replace && show_org);
    let mut show_preview = input.preview_active;

    let mut org_w = if show_org {
        input
            .org_sidebar_width
            .clamp(ORG_SIDEBAR_MIN_WIDTH, ORG_SIDEBAR_MAX_WIDTH)
    } else {
        0
    };
    let mut tree_w = if show_tree { input.file_tree_width } else { 0 };
    let mut preview_w = if show_preview { input.preview_width } else { 0 };
    let mut compact = false;

    // 1. Preview goes first — it is the most easily reopened panel and
    //    the one users treat as transient.
    if show_preview && width < MIN_PANE_AREA_WIDTH + org_w + tree_w + preview_w {
        show_preview = false;
        preview_w = 0;
    }
    // 2. Then the file tree. In `replace` mode it is already gone
    //    whenever the sidebar is up, so this only ever fires for
    //    `coexist` (or when the sidebar is off entirely, which is the
    //    pre-sidebar behaviour).
    if show_tree && width < MIN_PANE_AREA_WIDTH + org_w + tree_w {
        show_tree = false;
        tree_w = 0;
    }
    // 3. Squeeze the sidebar before giving up on it.
    if show_org && width < MIN_PANE_AREA_WIDTH + org_w && org_w > ORG_SIDEBAR_COMPACT_WIDTH {
        org_w = ORG_SIDEBAR_COMPACT_WIDTH;
        compact = true;
    }
    // 4. Last resort.
    if show_org && width < MIN_PANE_AREA_WIDTH + org_w {
        show_org = false;
        org_w = 0;
        compact = false;
    }

    // After the ladder the reserved widths always fit, but saturate
    // anyway: `render` bails out below 40 columns, yet `relayout_panes`
    // runs on the cached size and only guards `cols < 20`.
    let pane_w = width
        .saturating_sub(org_w)
        .saturating_sub(tree_w)
        .saturating_sub(preview_w);

    let y = input.area.y;
    let h = input.area.height;
    let mut x = input.area.x;

    let org_sidebar = show_org.then(|| {
        let r = Rect::new(x, y, org_w, h);
        x += org_w;
        r
    });
    let file_tree = show_tree.then(|| {
        let r = Rect::new(x, y, tree_w, h);
        x += tree_w;
        r
    });
    let mut preview = None;
    if show_preview && input.layout_swapped {
        preview = Some(Rect::new(x, y, preview_w, h));
        x += preview_w;
    }
    let panes = Rect::new(x, y, pane_w, h);
    x += pane_w;
    if show_preview && !input.layout_swapped {
        preview = Some(Rect::new(x, y, preview_w, h));
    }

    MainAreaLayout {
        org_sidebar,
        file_tree,
        preview,
        panes,
        org_sidebar_compact: compact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline: wide terminal, sidebar off, both legacy panels up.
    fn legacy(width: u16) -> MainAreaInput {
        MainAreaInput {
            area: Rect::new(0, 1, width, 30),
            org_sidebar_mode: OrgSidebarMode::Coexist,
            org_sidebar_visible: false,
            org_sidebar_width: DEFAULT_ORG_SIDEBAR_WIDTH,
            file_tree_visible: true,
            file_tree_width: 20,
            preview_active: true,
            preview_width: 40,
            layout_swapped: false,
        }
    }

    #[test]
    fn wide_terminal_keeps_every_panel_and_tiles_without_gaps() {
        let out = compute(MainAreaInput {
            org_sidebar_visible: true,
            ..legacy(160)
        });
        let org = out.org_sidebar.expect("sidebar fits at 160 cols");
        let tree = out.file_tree.expect("tree fits");
        let preview = out.preview.expect("preview fits");
        assert_eq!(org.x, 0);
        assert_eq!(tree.x, org.x + org.width);
        assert_eq!(out.panes.x, tree.x + tree.width);
        assert_eq!(preview.x, out.panes.x + out.panes.width);
        assert_eq!(preview.x + preview.width, 160);
        assert!(!out.org_sidebar_compact);
    }

    #[test]
    fn swapped_layout_puts_preview_left_of_panes_but_after_the_sidebars() {
        let out = compute(MainAreaInput {
            org_sidebar_visible: true,
            layout_swapped: true,
            ..legacy(160)
        });
        let org = out.org_sidebar.unwrap();
        let tree = out.file_tree.unwrap();
        let preview = out.preview.unwrap();
        assert_eq!(tree.x, org.x + org.width);
        assert_eq!(preview.x, tree.x + tree.width);
        assert_eq!(out.panes.x, preview.x + preview.width);
        assert_eq!(out.panes.x + out.panes.width, 160);
    }

    /// The whole point of the shared helper: with the sidebar disabled
    /// the ladder must reproduce the pre-sidebar behaviour exactly.
    #[test]
    fn sidebar_off_reproduces_legacy_degrade_order() {
        // 20 pane + 20 tree + 40 preview = 80 is the exact fit.
        assert!(compute(legacy(80)).preview.is_some());
        // One column short: preview goes, tree stays.
        let out = compute(legacy(79));
        assert!(out.preview.is_none());
        assert!(out.file_tree.is_some());
        assert_eq!(out.panes.width, 59);
        // Below 20 + 20 the tree goes too.
        let out = compute(legacy(39));
        assert!(out.file_tree.is_none());
        assert_eq!(out.panes.x, 0);
        assert_eq!(out.panes.width, 39);
    }

    #[test]
    fn preview_is_dropped_before_the_file_tree_and_the_tree_before_the_sidebar() {
        let base = MainAreaInput {
            org_sidebar_visible: true,
            ..legacy(160)
        };
        // 20 + 26 + 20 + 40 = 106 fits everything.
        assert!(compute(MainAreaInput {
            area: Rect::new(0, 1, 106, 30),
            ..base
        })
        .preview
        .is_some());

        // 105: preview drops, tree and sidebar survive.
        let out = compute(MainAreaInput {
            area: Rect::new(0, 1, 105, 30),
            ..base
        });
        assert!(out.preview.is_none());
        assert!(out.file_tree.is_some());
        assert!(out.org_sidebar.is_some());

        // 65 (< 20 + 26 + 20): tree drops, sidebar survives at full width.
        let out = compute(MainAreaInput {
            area: Rect::new(0, 1, 65, 30),
            ..base
        });
        assert!(out.file_tree.is_none());
        assert_eq!(out.org_sidebar.unwrap().width, DEFAULT_ORG_SIDEBAR_WIDTH);
        assert!(!out.org_sidebar_compact);
    }

    #[test]
    fn sidebar_shrinks_to_compact_before_it_is_dropped() {
        let base = MainAreaInput {
            org_sidebar_visible: true,
            file_tree_visible: false,
            preview_active: false,
            ..legacy(160)
        };
        // 45 >= 20 + 26 → full width.
        let out = compute(MainAreaInput {
            area: Rect::new(0, 1, 46, 30),
            ..base
        });
        assert_eq!(out.org_sidebar.unwrap().width, DEFAULT_ORG_SIDEBAR_WIDTH);

        // 45 < 20 + 26 → compact.
        let out = compute(MainAreaInput {
            area: Rect::new(0, 1, 45, 30),
            ..base
        });
        assert_eq!(out.org_sidebar.unwrap().width, ORG_SIDEBAR_COMPACT_WIDTH);
        assert!(out.org_sidebar_compact);
        assert_eq!(out.panes.width, 45 - ORG_SIDEBAR_COMPACT_WIDTH);

        // 35 < 20 + 16 → dropped entirely; panes take the whole area.
        let out = compute(MainAreaInput {
            area: Rect::new(0, 1, 35, 30),
            ..base
        });
        assert!(out.org_sidebar.is_none());
        assert!(!out.org_sidebar_compact);
        assert_eq!(out.panes, Rect::new(0, 1, 35, 30));
    }

    #[test]
    fn replace_mode_hides_the_file_tree_while_the_sidebar_is_up() {
        let base = MainAreaInput {
            org_sidebar_mode: OrgSidebarMode::Replace,
            org_sidebar_visible: true,
            ..legacy(160)
        };
        let out = compute(base);
        assert!(out.org_sidebar.is_some());
        assert!(out.file_tree.is_none(), "replace mode suppresses the tree");

        // Toggling the sidebar off hands the slot back to the tree.
        let out = compute(MainAreaInput {
            org_sidebar_visible: false,
            ..base
        });
        assert!(out.org_sidebar.is_none());
        assert!(out.file_tree.is_some());
    }

    #[test]
    fn off_mode_ignores_the_runtime_toggle() {
        let out = compute(MainAreaInput {
            org_sidebar_mode: OrgSidebarMode::Off,
            org_sidebar_visible: true,
            ..legacy(160)
        });
        assert!(out.org_sidebar.is_none());
        assert!(out.file_tree.is_some(), "off mode must not touch the tree");
    }

    #[test]
    fn sidebar_width_is_clamped_to_the_drag_bounds() {
        let base = MainAreaInput {
            org_sidebar_visible: true,
            file_tree_visible: false,
            preview_active: false,
            ..legacy(200)
        };
        let out = compute(MainAreaInput {
            org_sidebar_width: 2,
            ..base
        });
        assert_eq!(out.org_sidebar.unwrap().width, ORG_SIDEBAR_MIN_WIDTH);
        let out = compute(MainAreaInput {
            org_sidebar_width: 999,
            ..base
        });
        assert_eq!(out.org_sidebar.unwrap().width, ORG_SIDEBAR_MAX_WIDTH);
    }

    #[test]
    fn honours_a_non_zero_area_origin() {
        let out = compute(MainAreaInput {
            area: Rect::new(7, 3, 120, 20),
            org_sidebar_visible: true,
            ..legacy(120)
        });
        assert_eq!(out.org_sidebar.unwrap().x, 7);
        assert_eq!(out.org_sidebar.unwrap().y, 3);
        assert_eq!(out.panes.y, 3);
        assert_eq!(out.panes.height, 20);
        assert_eq!(out.preview.unwrap().x + out.preview.unwrap().width, 127);
    }
}
