use super::super::*;

// The Ctrl→Alt keybinding migration: every renga chord that used to
// live on a bare Ctrl+<letter> moved into the Alt namespace so the
// Ctrl bytes reach the PTY (shell readline, vim, fzf …) untouched.
// These tests pin both sides of that contract: Alt chords are
// consumed by renga, the freed Ctrl keys are not.

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn alt_d_splits_vertically_and_alt_e_horizontally() {
    let mut app = App::new(40, 120).expect("App::new");
    assert_eq!(app.ws().layout.pane_count(), 1);

    let consumed = app
        .handle_key_event(key(KeyCode::Char('d'), KeyModifiers::ALT))
        .expect("Alt+D");
    assert!(consumed, "Alt+D must be consumed as vertical split");
    assert_eq!(app.ws().layout.pane_count(), 2);

    let consumed = app
        .handle_key_event(key(KeyCode::Char('e'), KeyModifiers::ALT))
        .expect("Alt+E");
    assert!(consumed, "Alt+E must be consumed as horizontal split");
    assert_eq!(app.ws().layout.pane_count(), 3);
}

#[test]
fn alt_w_closes_focused_pane() {
    let mut app = App::new(40, 120).expect("App::new");
    app.handle_key_event(key(KeyCode::Char('d'), KeyModifiers::ALT))
        .expect("split");
    assert_eq!(app.ws().layout.pane_count(), 2);

    let consumed = app
        .handle_key_event(key(KeyCode::Char('w'), KeyModifiers::ALT))
        .expect("Alt+W");
    assert!(consumed, "Alt+W must be consumed as close-pane");
    assert_eq!(app.ws().layout.pane_count(), 1);
}

#[test]
fn alt_q_quits_even_with_overlay_open() {
    let mut app = App::new(40, 80).expect("App::new");
    let consumed = app
        .handle_key_event(key(KeyCode::Char('q'), KeyModifiers::ALT))
        .expect("Alt+Q");
    assert!(consumed);
    assert!(app.should_quit, "Alt+Q must quit");

    // Escape hatch: must fire before overlay routing.
    let mut app = App::new(40, 80).expect("App::new");
    let pane = app.ws().focused_pane_id;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane));
    let consumed = app
        .handle_key_event(key(KeyCode::Char('q'), KeyModifiers::ALT))
        .expect("Alt+Q with overlay");
    assert!(consumed);
    assert!(
        app.should_quit,
        "Alt+Q must quit even while the IME overlay holds input"
    );
}

#[test]
fn alt_f_toggles_file_tree_and_alt_o_swaps_layout() {
    let mut app = App::new(40, 120).expect("App::new");
    // The tree starts visible with focus on the pane; the first
    // Alt+F moves focus onto the tree, the second (handled by the
    // file-tree-mode branch) hides it.
    assert!(app.ws().file_tree_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);

    let consumed = app
        .handle_key_event(key(KeyCode::Char('f'), KeyModifiers::ALT))
        .expect("Alt+F");
    assert!(consumed, "Alt+F must be consumed as file-tree toggle");
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);

    let consumed = app
        .handle_key_event(key(KeyCode::Char('f'), KeyModifiers::ALT))
        .expect("Alt+F again");
    assert!(consumed);
    assert!(!app.ws().file_tree_visible);
    assert_eq!(app.ws().focus_target, FocusTarget::Pane);

    let before = app.layout_swapped;
    let consumed = app
        .handle_key_event(key(KeyCode::Char('o'), KeyModifiers::ALT))
        .expect("Alt+O");
    assert!(consumed, "Alt+O must be consumed as layout swap");
    assert_ne!(app.layout_swapped, before, "Alt+O must flip the layout");
}

#[test]
fn alt_up_down_cycle_pane_focus() {
    let mut app = App::new(40, 120).expect("App::new");
    app.handle_key_event(key(KeyCode::Char('d'), KeyModifiers::ALT))
        .expect("split");
    // The split focuses the new (last) pane, so cycle backwards
    // first — cycling forward from the last pane would hand focus
    // to the sidebar, not another pane.
    let after_split = app.ws().focused_pane_id;

    let consumed = app
        .handle_key_event(key(KeyCode::Up, KeyModifiers::ALT))
        .expect("Alt+Up");
    assert!(consumed, "Alt+Up must be consumed as prev-pane focus");
    let prev = app.ws().focused_pane_id;
    assert_ne!(prev, after_split, "Alt+Up must move focus");

    let consumed = app
        .handle_key_event(key(KeyCode::Down, KeyModifiers::ALT))
        .expect("Alt+Down");
    assert!(consumed, "Alt+Down must be consumed as next-pane focus");
    assert_eq!(
        app.ws().focused_pane_id,
        after_split,
        "Alt+Down must move focus back"
    );
}

#[test]
fn freed_ctrl_keys_fall_through_to_the_pty() {
    // The whole point of the migration: these must NOT be consumed
    // so the main loop forwards their bytes to the focused PTY.
    let mut app = App::new(40, 120).expect("App::new");

    for (code, name) in [
        (KeyCode::Char('d'), "Ctrl+D"),
        (KeyCode::Char('e'), "Ctrl+E"),
        (KeyCode::Char('t'), "Ctrl+T"),
        (KeyCode::Char('f'), "Ctrl+F"),
        (KeyCode::Char('p'), "Ctrl+P"),
        (KeyCode::Char('q'), "Ctrl+Q"),
        (KeyCode::Right, "Ctrl+Right"),
        (KeyCode::Left, "Ctrl+Left"),
    ] {
        let consumed = app
            .handle_key_event(key(code, KeyModifiers::CONTROL))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!consumed, "{name} must fall through to the PTY");
    }
    assert!(!app.should_quit, "Ctrl+Q must no longer quit");
    assert_eq!(
        app.ws().layout.pane_count(),
        1,
        "no Ctrl key may split panes anymore"
    );

    // Ctrl+W on a lone pane in a lone tab already returned false
    // before the migration (nothing to close); after it, it must
    // still fall through even with two panes present.
    app.handle_key_event(key(KeyCode::Char('d'), KeyModifiers::ALT))
        .expect("split");
    assert_eq!(app.ws().layout.pane_count(), 2);
    let consumed = app
        .handle_key_event(key(KeyCode::Char('w'), KeyModifiers::CONTROL))
        .expect("Ctrl+W");
    assert!(!consumed, "Ctrl+W must fall through to the PTY");
    assert_eq!(
        app.ws().layout.pane_count(),
        2,
        "Ctrl+W must not close panes"
    );
}

// -- direct pane scrolling (Alt+PageUp/PageDown/Home/End) --------

/// Seed the focused pane with a rendered rect and enough vt100
/// scrollback to scroll through. Returns the pane id.
fn seed_scrollback(app: &mut App) -> usize {
    let pane_id = app.ws().focused_pane_id;
    // 12-row frame → 10 content rows after the borders.
    app.ws_mut().last_pane_rects = vec![(pane_id, ratatui::layout::Rect::new(0, 0, 40, 12))];
    let pane = app.ws().panes.get(&pane_id).expect("focused pane");
    // Let the real shell finish printing its startup banner/prompt
    // first: vt100 auto-shifts a scrolled-back offset when new lines
    // arrive, so late shell output between two assertions would skew
    // the exact-offset checks below.
    let start = std::time::Instant::now();
    while !pane.prompt_seen.load(std::sync::atomic::Ordering::Relaxed)
        && start.elapsed() < std::time::Duration::from_secs(5)
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..100 {
        parser.process(format!("line {i}\r\n").as_bytes());
    }
    pane_id
}

fn scroll_offset(app: &App, pane_id: usize) -> usize {
    app.ws()
        .panes
        .get(&pane_id)
        .expect("pane")
        .parser
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .screen()
        .scrollback()
}

#[test]
fn alt_page_home_end_scroll_the_focused_pane() {
    let mut app = App::new(40, 120).expect("App::new");
    let pane_id = seed_scrollback(&mut app);
    assert_eq!(scroll_offset(&app, pane_id), 0);

    let consumed = app
        .handle_key_event(key(KeyCode::PageUp, KeyModifiers::ALT))
        .expect("Alt+PageUp");
    assert!(consumed, "Alt+PageUp must be consumed as pane scroll");
    assert_eq!(
        scroll_offset(&app, pane_id),
        10,
        "one page = pane content height"
    );

    let consumed = app
        .handle_key_event(key(KeyCode::PageDown, KeyModifiers::ALT))
        .expect("Alt+PageDown");
    assert!(consumed, "Alt+PageDown must be consumed as pane scroll");
    assert_eq!(scroll_offset(&app, pane_id), 0);

    let consumed = app
        .handle_key_event(key(KeyCode::Home, KeyModifiers::ALT))
        .expect("Alt+Home");
    assert!(consumed, "Alt+Home must be consumed as pane scroll");
    // 100 seeded lines minus the visible screen — the exact value
    // depends on the vt100 size (and any concurrent shell output),
    // but it is always well past one page.
    assert!(
        scroll_offset(&app, pane_id) > 10,
        "Alt+Home must jump to the top of history"
    );

    let consumed = app
        .handle_key_event(key(KeyCode::End, KeyModifiers::ALT))
        .expect("Alt+End");
    assert!(consumed, "Alt+End must be consumed as pane scroll");
    assert_eq!(
        scroll_offset(&app, pane_id),
        0,
        "Alt+End must return to the live view"
    );

    // Without Alt the keys must still fall through to the PTY.
    for (code, name) in [
        (KeyCode::PageUp, "PageUp"),
        (KeyCode::PageDown, "PageDown"),
        (KeyCode::Home, "Home"),
        (KeyCode::End, "End"),
    ] {
        let consumed = app
            .handle_key_event(key(code, KeyModifiers::NONE))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!consumed, "bare {name} must reach the PTY");
    }
    assert_eq!(scroll_offset(&app, pane_id), 0);
}

#[test]
fn alt_page_scroll_is_skipped_when_sidebar_has_focus() {
    let mut app = App::new(40, 120).expect("App::new");
    let pane_id = seed_scrollback(&mut app);

    app.handle_key_event(key(KeyCode::Char('f'), KeyModifiers::ALT))
        .expect("Alt+F");
    assert_eq!(app.ws().focus_target, FocusTarget::FileTree);

    app.handle_key_event(key(KeyCode::PageUp, KeyModifiers::ALT))
        .expect("Alt+PageUp");
    assert_eq!(
        scroll_offset(&app, pane_id),
        0,
        "sidebar focus must not scroll the hidden pane"
    );
}

#[test]
fn ctrl_c_conditional_copy_is_unchanged() {
    // Out of scope for the migration: Ctrl+C with no selection must
    // still fall through (SIGINT), exactly as before.
    let mut app = App::new(40, 80).expect("App::new");
    let consumed = app
        .handle_key_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("Ctrl+C");
    assert!(!consumed, "Ctrl+C without selection must reach the PTY");
}
