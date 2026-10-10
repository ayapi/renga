use super::super::*;

use crate::app::layout_geometry;
use crate::app::org_sidebar::OrgSidebarTarget;
use crate::config::OrgSidebarMode;

/// `App::new` leaves the sidebar off (it is turned on by
/// `apply_config`), so every test here opts in explicitly.
fn app_with_sidebar(rows: u16, cols: u16) -> App {
    let mut app = App::new(rows, cols).expect("App::new");
    app.last_term_size = (cols, rows);
    app.org_sidebar_mode = OrgSidebarMode::Coexist;
    app.org_sidebar_visible = true;
    app
}

fn add_tabs(app: &mut App, n: usize) {
    for _ in 0..n {
        app.new_tab().expect("new_tab");
    }
}

// ─── switch_tab ───────────────────────────────────────────

#[test]
fn switch_tab_carries_org_sidebar_focus_into_the_incoming_tab() {
    // `focus_target` is per-workspace, so without an explicit carry
    // the incoming tab restores whatever it was focused on last —
    // knocking the user out of the very panel they are driving.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;

    assert!(app.switch_tab(1), "switch to a different tab should apply");
    assert_eq!(
        app.ws().focus_target,
        FocusTarget::OrgSidebar,
        "sidebar focus must survive a tab switch"
    );
}

#[test]
fn switch_tab_leaves_non_sidebar_focus_to_the_incoming_tab() {
    // The carry is deliberately sidebar-only: the file tree and
    // preview are per-tab panels, so each tab keeps its own answer.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::FileTree;
    app.switch_tab(1);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn switch_tab_rejects_an_out_of_range_index() {
    // `ws()` indexes `workspaces` directly and would panic. The
    // sidebar renders from a snapshot an MCP `close_tab` can
    // invalidate before the click lands.
    let mut app = app_with_sidebar(40, 160);
    assert!(!app.switch_tab(7));
    assert_eq!(app.active_tab, 0);
}

#[test]
fn switch_tab_to_the_current_tab_is_a_noop() {
    // The old keyboard paths called `suspend_overlay` unconditionally,
    // so Alt+1 on the tab you were already on stashed the IME draft
    // for no reason.
    let mut app = app_with_sidebar(40, 160);
    app.last_tab_click = Some((0, Instant::now()));
    assert!(!app.switch_tab(0));
    assert!(
        app.last_tab_click.is_some(),
        "a no-op switch must not clear caches"
    );
}

#[test]
fn switch_tab_clears_every_tab_keyed_cache() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    let now = Instant::now();
    app.last_tab_click = Some((0, now));
    app.last_edge_click = Some((0, 5, 5, now));
    app.last_boundary_click = Some((0, 5, 5, now));

    app.switch_tab(1);

    assert!(app.last_tab_click.is_none());
    assert!(app.last_edge_click.is_none());
    assert!(app.last_boundary_click.is_none());
    assert!(app.selection.is_none());
}

// ─── close_tab index shift ────────────────────────────────

#[test]
fn closing_a_tab_before_the_active_one_follows_the_index_shift() {
    // Regression guard. `workspaces.remove(index)` shifts every later
    // tab down one slot, but the old code only clamped when
    // `active_tab` ran off the end. With four tabs and the user on
    // index 2, closing index 1 left `active_tab` numerically 2 —
    // which now pointed at what used to be tab 3. The user silently
    // ended up on a workspace they never selected.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 3); // tabs 0..=3
    app.switch_tab(2);
    let watching = app.ws().focused_pane_id;

    app.close_tab(1);

    assert_eq!(app.workspaces.len(), 3);
    assert_eq!(app.active_tab, 1, "active tab must follow the shift");
    assert_eq!(
        app.ws().focused_pane_id,
        watching,
        "the user must still be looking at the same workspace"
    );
}

#[test]
fn closing_a_tab_after_the_active_one_leaves_the_active_index_alone() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 3);
    app.switch_tab(1);
    let watching = app.ws().focused_pane_id;

    app.close_tab(3);

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focused_pane_id, watching);
}

#[test]
fn closing_the_active_tab_lands_on_the_tab_that_takes_its_slot() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 2); // tabs 0..=2
    app.switch_tab(1);
    let next_tab_pane = app.workspaces[2].focused_pane_id;

    app.close_tab(1);

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focused_pane_id, next_tab_pane);
}

#[test]
fn tab_set_changes_drop_the_sidebar_click_cache() {
    // Row indices and tab indices both shift underneath the cache.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 2);
    app.org_sidebar_row_targets = vec![OrgSidebarTarget::tab(0)];
    app.org_sidebar_scroll = 4;
    app.org_sidebar_selection = Some(OrgSidebarTarget::tab(2));

    app.close_tab(2);

    assert!(app.org_sidebar_row_targets.is_empty());
    assert_eq!(app.org_sidebar_scroll, 0);
    assert!(app.org_sidebar_selection.is_none());

    app.org_sidebar_scroll = 3;
    app.new_tab().expect("new_tab");
    assert_eq!(app.org_sidebar_scroll, 0);
}

// ─── focus cycle ──────────────────────────────────────────

#[test]
fn focus_cycle_visits_the_sidebar_last() {
    // Order is `panes → file tree → preview → org sidebar → panes`;
    // the sidebar goes on the end so existing Ctrl+Right muscle
    // memory is untouched.
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().file_tree_visible = true;
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);

    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    app.focus_next_pane(); // preview is inactive, so straight on
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn focus_cycle_runs_backwards_symmetrically() {
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().file_tree_visible = true;

    app.focus_prev_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
    app.focus_prev_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    app.focus_prev_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn focus_cycle_skips_the_sidebar_when_it_is_off() {
    // The pre-#291 cycle must be reproduced exactly for users who
    // never turn the panel on.
    let mut app = App::new(40, 160).expect("App::new");
    app.ws_mut().file_tree_visible = true;

    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn focus_leaves_a_sidebar_that_disappeared_underneath_it() {
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;
    app.org_sidebar_visible = false;

    app.focus_next_pane();
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

// ─── row model ────────────────────────────────────────────

#[test]
fn rows_list_tabs_then_their_panes_in_layout_order() {
    // Pane order comes from `collect_pane_ids` (the layout tree's own
    // first-then-second walk), not `HashMap` iteration, so the list
    // matches what is on screen.
    let mut app = app_with_sidebar(40, 160);
    app.split_focused_pane(SplitDirection::Vertical, None)
        .expect("split");
    add_tabs(&mut app, 1);

    let expected_tab0: Vec<usize> = app.workspaces[0].layout.collect_pane_ids();
    assert_eq!(expected_tab0.len(), 2, "tab 0 should have two panes");

    let rows = app.org_sidebar_rows();
    let targets: Vec<OrgSidebarTarget> = rows.iter().map(|r| r.target).collect();

    assert_eq!(targets[0], OrgSidebarTarget::tab(0));
    assert_eq!(targets[1].pane_id, Some(expected_tab0[0]));
    assert_eq!(targets[2].pane_id, Some(expected_tab0[1]));
    assert_eq!(targets[3], OrgSidebarTarget::tab(1));
}

#[test]
fn selection_is_stored_by_identity_not_row_index() {
    // A row index would silently retarget when a tab above the
    // selection is closed.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 2);
    app.org_sidebar_selection = Some(OrgSidebarTarget::tab(2));

    let rows = app.org_sidebar_rows();
    let before = app.org_sidebar_selected_index(&rows);
    assert_eq!(rows[before].target, OrgSidebarTarget::tab(2));
}

#[test]
fn a_stale_selection_falls_back_to_the_active_tab_row() {
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_selection = Some(OrgSidebarTarget {
        tab: 9,
        pane_id: Some(999),
    });

    let rows = app.org_sidebar_rows();
    let idx = app.org_sidebar_selected_index(&rows);
    assert_eq!(rows[idx].target, OrgSidebarTarget::tab(app.active_tab));
}

#[test]
fn activating_a_pane_row_switches_tab_and_focuses_that_pane() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    let other_pane = app.workspaces[1].focused_pane_id;
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;

    app.org_sidebar_activate_target(OrgSidebarTarget {
        tab: 1,
        pane_id: Some(other_pane),
    });

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focused_pane_id, other_pane);
    assert_eq!(
        app.ws().focus_target,
        FocusTarget::Pane,
        "a pane row is an explicit 'take me there'"
    );
}

#[test]
fn activating_a_tab_row_keeps_focus_in_the_sidebar() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 1);
    app.switch_tab(0);
    app.ws_mut().focus_target = FocusTarget::OrgSidebar;

    app.org_sidebar_activate_target(OrgSidebarTarget::tab(1));

    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
}

#[test]
fn activating_a_vanished_tab_is_ignored() {
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_activate_target(OrgSidebarTarget::tab(5));
    assert_eq!(app.active_tab, 0);
}

// ─── toggle / modes ───────────────────────────────────────

#[test]
fn toggle_is_inert_when_the_mode_is_off() {
    // `off` frees Ctrl+B for the PTY, the same way `[ime] mode = off`
    // frees Ctrl+;.
    let mut app = App::new(40, 160).expect("App::new");
    app.org_sidebar_mode = OrgSidebarMode::Off;
    app.org_sidebar_visible = false;

    app.toggle_org_sidebar();

    assert!(!app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn toggle_follows_the_file_tree_three_way_shape() {
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_visible = false;

    app.toggle_org_sidebar(); // hidden → shown + focused
    assert!(app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);

    app.ws_mut().focus_target = FocusTarget::Pane;
    app.toggle_org_sidebar(); // shown but unfocused → focus only
    assert!(app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);

    app.toggle_org_sidebar(); // shown and focused → hide
    assert!(!app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);
}

#[test]
fn replace_mode_hands_the_slot_between_the_two_panels() {
    // The panels share one column range in `replace`, so Ctrl+F has to
    // close the sidebar — otherwise the tree would take focus while
    // the layout kept painting the sidebar over it.
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_mode = OrgSidebarMode::Replace;
    app.ws_mut().file_tree_visible = true;

    // Sidebar up ⇒ the tree is not painted, so Ctrl+F must reopen it.
    app.toggle_file_tree();
    assert!(
        !app.org_sidebar_visible,
        "opening the tree closes the panel"
    );
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    assert!(app.ws().file_tree_visible);
}

// ─── self-review regressions ──────────────────────────────
//
// Each test below pins a defect the adversarial self-review pass
// confirmed against the first cut of this feature.

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[test]
fn focus_cycle_skips_a_file_tree_that_replace_mode_is_hiding() {
    // `focus_cycle_targets` read the raw `file_tree_visible` flag, so
    // in `replace` mode — where the sidebar holds the tree's slot and
    // the tree is not painted at all — a single Ctrl+Right parked
    // focus on an invisible panel. Every later keystroke was then
    // swallowed by `handle_file_tree_key`, and a bare `c` / `v` split
    // the workspace into a new Claude pane the user never asked for.
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_mode = OrgSidebarMode::Replace;
    app.ws_mut().file_tree_visible = true;
    assert!(!app.file_tree_painted(), "replace mode hides the tree");

    app.focus_next_pane();
    assert_eq!(
        app.ws().focus_target,
        FocusTarget::OrgSidebar,
        "the cycle must skip the unpainted tree"
    );
}

#[test]
fn focus_cycle_skips_panels_the_narrow_terminal_degrade_ladder_dropped() {
    // The sibling of the `replace` case, and the one that made the
    // hazard routine: the sidebar ships on by default, so it eats
    // columns the tree and preview used to have and they now get
    // dropped at widths where they previously survived. Cycle
    // membership therefore has to come from the resolved layout, not
    // from `file_tree_visible` / `preview.is_active()`.
    let mut app = app_with_sidebar(40, 60);
    app.ws_mut().file_tree_visible = true;
    // Setting `file_path` is what `is_active()` reads; going through
    // `Preview::load` would drag in a Picker and the message table for
    // no benefit here (mirrors the ipc_state rect tests).
    app.ws_mut().preview.file_path = Some(std::path::PathBuf::from("Cargo.toml"));

    let layout = app.main_area_layout();
    assert!(app.ws().preview.is_active(), "preview is logically on");
    assert!(layout.preview.is_none(), "…but 60 cols cannot fit it");
    assert!(layout.file_tree.is_none(), "…nor the tree");
    assert!(layout.org_sidebar.is_some(), "the sidebar survives");

    app.focus_next_pane();
    assert_eq!(
        app.ws().focus_target,
        FocusTarget::OrgSidebar,
        "the cycle must skip both dropped panels"
    );
}

#[test]
fn keys_do_not_route_to_a_panel_the_degrade_ladder_dropped() {
    let mut app = app_with_sidebar(40, 60);
    app.ws_mut().file_tree_visible = true;
    app.ws_mut().focus_target = FocusTarget::FileTree;
    assert!(!app.file_tree_painted());
    let panes_before = app.ws().layout.pane_count();

    app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .expect("handle_key_event");

    assert_eq!(app.ws().layout.pane_count(), panes_before);
}

#[test]
fn ctrl_f_never_focuses_a_tree_the_terminal_is_too_narrow_to_show() {
    // On a terminal that cannot fit the tree, Ctrl+F has nothing to
    // show, so it leaves the flag alone and — the part that matters —
    // does not hand focus to a panel with no cells on screen. Widening
    // the window restores the ordinary behaviour.
    let mut app = app_with_sidebar(40, 60);
    app.ws_mut().file_tree_visible = true;
    assert!(
        !app.file_tree_painted(),
        "60 cols cannot fit sidebar + tree"
    );

    app.toggle_file_tree();
    assert_ne!(
        app.ws().focus_target,
        FocusTarget::FileTree,
        "focus must not land on an unpainted panel"
    );
    assert!(app.ws().file_tree_visible, "the flag is left set");

    // Same key, room to draw: normal three-way behaviour returns.
    app.last_term_size = (160, 40);
    assert!(app.file_tree_painted());
    app.toggle_file_tree();
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);
    app.toggle_file_tree();
    assert!(!app.ws().file_tree_visible);
}

#[test]
fn keys_do_not_route_to_a_file_tree_that_replace_mode_is_hiding() {
    // Belt and braces for the above: even if focus lands on the tree
    // some other way, the dispatch must not hand it the keyboard.
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_mode = OrgSidebarMode::Replace;
    app.ws_mut().file_tree_visible = true;
    app.ws_mut().focus_target = FocusTarget::FileTree;
    let panes_before = app.ws().layout.pane_count();

    // `c` is "split a Claude pane here" inside the file tree.
    app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .expect("handle_key_event");

    assert_eq!(
        app.ws().layout.pane_count(),
        panes_before,
        "an unpainted tree must not act on bare character keys"
    );
}

#[test]
fn ctrl_b_passes_through_to_the_pty_when_the_mode_is_off() {
    // `off` is the documented escape hatch for users who need Ctrl+B
    // in readline / vim / a nested tmux. The first cut consumed the
    // key unconditionally and merely made the toggle a no-op, so the
    // escape hatch did nothing — and both keymap docs plus the
    // function's own doc comment claimed the opposite.
    let mut app = App::new(40, 160).expect("App::new");
    app.org_sidebar_mode = OrgSidebarMode::Off;
    app.org_sidebar_visible = false;

    let consumed = app.handle_key_event(ctrl('b')).expect("handle_key_event");

    assert!(
        !consumed,
        "Ctrl+B must reach the PTY when [ui] org_sidebar = off"
    );
    assert!(!app.org_sidebar_visible);
}

#[test]
fn ctrl_b_is_consumed_when_the_sidebar_is_enabled() {
    let mut app = app_with_sidebar(40, 160);
    app.org_sidebar_visible = false;

    let consumed = app.handle_key_event(ctrl('b')).expect("handle_key_event");

    assert!(consumed);
    assert!(app.org_sidebar_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
}

#[test]
fn ctrl_b_reaches_the_sidebar_from_file_tree_and_preview_focus() {
    // Ctrl+F works from inside the file tree, so Ctrl+B has to work
    // from inside the other panels too. It used to sit *after* the
    // per-panel dispatch, where `handle_file_tree_key` had already
    // swallowed it.
    for focus in [FocusTarget::FileTree, FocusTarget::Preview] {
        let mut app = app_with_sidebar(40, 160);
        app.org_sidebar_visible = false;
        app.ws_mut().file_tree_visible = true;
        app.ws_mut().focus_target = focus;

        app.handle_key_event(ctrl('b')).expect("handle_key_event");

        assert!(
            app.org_sidebar_visible,
            "Ctrl+B should open the sidebar from {focus:?} focus"
        );
        assert_eq!(app.ws().focus_target, FocusTarget::OrgSidebar);
    }
}

#[test]
fn the_wheel_scroll_position_survives_the_next_paint() {
    // The renderer re-anchored the view on the selected row every
    // frame, so a wheel scroll was undone before it was ever drawn and
    // the panel simply refused to move.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 5);
    // `new_tab` leaves the last tab active; go back to the top so the
    // default selection is row 0 and any surviving scroll is unambiguous.
    app.switch_tab(0);
    let rows = app.org_sidebar_rows().len();
    assert!(rows > 4, "need more rows than the viewport for this test");

    app.org_sidebar_follow_selection = false;
    app.org_sidebar_scroll = 3;
    let selected = app.org_sidebar_selected_index(&app.org_sidebar_rows());
    assert_eq!(selected, 0, "selection is still the first tab header");

    app.org_sidebar_ensure_visible(selected, 4, rows);

    assert_eq!(
        app.org_sidebar_scroll, 3,
        "a paint must not drag the view back to the selection"
    );
}

#[test]
fn moving_the_selection_does_pull_the_view_back() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 5);
    let rows = app.org_sidebar_rows().len();
    app.org_sidebar_scroll = 6;

    app.org_sidebar_move_selection(-100); // jump to the top
    let selected = app.org_sidebar_selected_index(&app.org_sidebar_rows());
    app.org_sidebar_ensure_visible(selected, 4, rows);

    assert_eq!(selected, 0);
    assert_eq!(app.org_sidebar_scroll, 0);
}

#[test]
fn scroll_is_clamped_when_rows_disappear() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 5);
    app.org_sidebar_follow_selection = false;
    app.org_sidebar_scroll = 8;

    // Same viewport, but only 4 rows left to show.
    app.org_sidebar_ensure_visible(0, 4, 4);

    assert_eq!(app.org_sidebar_scroll, 0);
}

#[test]
fn closing_a_background_tab_leaves_the_ime_overlay_alone() {
    // The first cut suspended the overlay whenever `active_tab`
    // changed numerically — which, now that an earlier close
    // decrements it, includes the case where the user's own workspace
    // is untouched. Tearing down a half-composed overlay because some
    // other tab closed in the background is its own regression.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 3);
    app.switch_tab(2);
    let target_pane = app.ws().focused_pane_id;
    app.overlay = Some(crate::input::overlay::OverlayState::new(target_pane));

    app.close_tab(0);

    assert_eq!(app.active_tab, 1, "still the same workspace");
    assert!(
        app.overlay.is_some(),
        "a background close must not suspend the overlay"
    );
}

#[test]
fn closing_the_active_tab_still_suspends_the_ime_overlay() {
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 2);
    app.switch_tab(0);
    // Target a pane in a *different* tab so `close_tab`'s own
    // "overlay belonged to the closed tab" path doesn't fire and we
    // observe the suspend decision itself.
    let other_pane = app.workspaces[2].focused_pane_id;
    app.overlay = Some(crate::input::overlay::OverlayState::new(other_pane));

    app.close_tab(0);

    assert!(
        app.overlay.is_none(),
        "closing the tab under the user suspends the overlay"
    );
    assert!(app.saved_overlay_drafts.contains_key(&other_pane));
}

#[test]
fn background_status_changes_do_not_pierce_the_ime_overlay_freeze() {
    // `tick_claude_snapshots` runs outside the `dirty` gate on purpose,
    // but it must still respect the freeze that keeps composition from
    // flickering — otherwise a background tab's Claude churn repaints
    // the panes at up to 4 Hz behind the overlay.
    let mut app = app_with_sidebar(40, 160);
    app.ime_freeze_panes_on_overlay = true;
    let pane = app.ws().focused_pane_id;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane));
    // Seed a snapshot that disagrees with the (empty) monitor state so
    // the sweep is guaranteed to see a change.
    app.claude_snapshots.insert(
        pane,
        crate::claude_monitor::ClaudeSnapshot {
            is_working: true,
            ..Default::default()
        },
    );
    app.dirty = false;

    app.tick_claude_snapshots();

    assert!(
        !app.dirty,
        "the freeze must hold while the overlay is composing"
    );
    assert_eq!(
        app.claude_snapshots.get(&pane).map(|s| s.is_working),
        Some(false),
        "the cache is still refreshed — only the repaint is deferred"
    );
}

#[test]
fn clicking_the_sidebar_border_rows_does_not_activate_a_row() {
    // The hit test accepted the whole rect while the row index
    // saturated at the top border and ran one past the viewport at the
    // bottom one. Clicking the " ORG " title jumped to whatever was at
    // the top of the view, and clicking the bottom border jumped to a
    // row that had never been on screen — both of which switch tabs
    // and move pane focus.
    let mut app = app_with_sidebar(40, 160);
    add_tabs(&mut app, 3);
    app.switch_tab(3);
    app.relayout_panes();

    // A 6-row panel: border, four inner rows, border.
    let rect = Rect::new(0, 1, 26, 6);
    app.last_org_sidebar_rect = Some(rect);
    app.org_sidebar_row_targets = app.org_sidebar_rows().iter().map(|r| r.target).collect();
    app.org_sidebar_scroll = 0;
    assert!(
        app.org_sidebar_row_targets.len() > 4,
        "need rows past the viewport for the bottom-border case"
    );

    for border_row in [rect.y, rect.y + rect.height - 1] {
        app.switch_tab(3);
        app.handle_mouse_event(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: border_row,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.active_tab, 3,
            "clicking border row {border_row} must not switch tabs"
        );
    }

    // The inner rows still work.
    app.handle_mouse_event(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 5,
        row: rect.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.active_tab, 0, "the first inner row is tab 1's header");
}

#[test]
fn the_file_tree_border_drag_recovers_after_the_tree_degrades_away() {
    // The drag handler was gated on `last_file_tree_rect`, which the
    // renderer sets to `None` as soon as a wide-enough drag trips the
    // degrade ladder. The width then froze at its widest value: the
    // tree could not be dragged back, its border no longer hit-tested,
    // and only a terminal resize got it back.
    let mut app = App::new(40, 75).expect("App::new");
    app.last_term_size = (75, 40);
    app.ws_mut().file_tree_visible = true;
    app.ws_mut().last_file_tree_rect = None; // as if degraded out
    app.dragging = Some(DragTarget::FileTreeBorder);

    app.handle_mouse_event(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 30,
        row: 10,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(
        app.file_tree_width, 30,
        "dragging back left must still narrow the tree"
    );
}

// ─── geometry agreement ───────────────────────────────────

#[test]
fn relayout_places_panes_past_the_sidebar_and_the_tree() {
    // `render_main_area` and `relayout_panes` now resolve geometry
    // through the same helper; this pins the observable half of that
    // contract — the origin `renga list` and mouse hit-testing read
    // out of `last_pane_rects`.
    let mut app = app_with_sidebar(40, 160);
    app.ws_mut().file_tree_visible = true;
    let expected_x = app.org_sidebar_width + app.file_tree_width;

    app.relayout_panes();

    let pane_id = app.ws().focused_pane_id;
    let rect = app
        .ws()
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| *r)
        .expect("relayout should populate the focused pane's rect");
    assert_eq!(rect.x, expected_x);
    assert_eq!(rect.y, 1, "still below the tab strip");
}

#[test]
fn relayout_matches_the_layout_helper_across_narrow_widths() {
    // Walk the whole degrade ladder and assert the PTY-side origin
    // agrees with the helper at every step, including the widths
    // where a panel drops out.
    for cols in [160u16, 106, 105, 66, 65, 46, 45, 36, 35, 24] {
        let mut app = app_with_sidebar(40, cols);
        app.ws_mut().file_tree_visible = true;
        app.relayout_panes();

        let expected = layout_geometry::compute(app.main_area_input(Rect::new(0, 1, cols, 39)));
        let pane_id = app.ws().focused_pane_id;
        let rect = app
            .ws()
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)
            .expect("focused pane rect");
        assert_eq!(
            rect.x, expected.panes.x,
            "pane origin drifted from the shared helper at {cols} cols"
        );
        assert_eq!(
            rect.width, expected.panes.width,
            "pane width drifted from the shared helper at {cols} cols"
        );
    }
}

#[test]
fn a_hidden_sidebar_reproduces_the_pre_291_pane_origin() {
    // Back-compat guard: users who never enable the panel must see
    // byte-identical geometry.
    let mut app = App::new(40, 120).expect("App::new");
    app.last_term_size = (120, 40);
    app.ws_mut().file_tree_visible = true;
    let tree_w = app.file_tree_width;

    app.relayout_panes();

    let pane_id = app.ws().focused_pane_id;
    let rect = app
        .ws()
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| *r)
        .expect("focused pane rect");
    assert_eq!(rect.x, tree_w);
}
