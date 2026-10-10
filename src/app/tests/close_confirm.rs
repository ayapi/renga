//! Ctrl+W close confirmation (Issue #285).
//!
//! The prompt guards the only keystroke in renga that kills a running
//! process, so these tests pin down three things that are easy to
//! regress: the modal never leaks input to the PTY, the MCP path is
//! untouched by it, and `y` closes the target that was pinned at
//! request time — not whatever happens to be focused a second later.

use super::super::*;

fn make_pane_spec(id: &str) -> crate::layout_config::LayoutNodeSpec {
    crate::layout_config::LayoutNodeSpec::Pane {
        id: id.to_string(),
        command: None,
        role: None,
        cwd: None,
    }
}

/// App with one tab holding two panes, named "left" / "right".
/// Focus lands on "right" (the freshly split pane).
fn app_with_two_panes() -> App {
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "close-confirm".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(make_pane_spec("left")),
            second: Box::new(make_pane_spec("right")),
        },
    };
    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");
    app
}

/// Two side-by-side panes with pane rects published the way the
/// renderer publishes them, so pointer coordinates in these tests
/// resolve against real geometry. Mirrors the helper in
/// `tests::pointer_input`; the divider sits at column 50.
fn two_pane_app_with_rects() -> (App, usize, usize) {
    let mut app = App::new(40, 100).expect("App::new");
    let left = app.ws().focused_pane_id;
    app.split_focused_pane_with_position(SplitDirection::Vertical, false, None)
        .expect("split succeeds")
        .expect("split not refused");
    let right = app.ws().focused_pane_id;
    assert_ne!(left, right);
    let rects = app.ws().layout.calculate_rects(Rect::new(0, 0, 100, 40));
    app.ws_mut().last_pane_rects = rects;
    (app, left, right)
}

fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// A cell well inside the given pane's published rect.
fn interior_of(app: &App, pane: usize) -> (u16, u16) {
    let rect = app
        .ws()
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == pane)
        .map(|&(_, r)| r)
        .expect("pane rect published");
    assert!(
        rect.width > 2 && rect.height > 2,
        "pane must have an interior to click"
    );
    (rect.x + rect.width / 2, rect.y + rect.height / 2)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn pane_id(app: &App, name: &str) -> usize {
    *app.ws()
        .pane_names
        .get(name)
        .unwrap_or_else(|| panic!("pane {name} registered"))
}

// ─── Request phase ────────────────────────────────────────

#[test]
fn ctrl_w_asks_before_closing_a_pane() {
    let mut app = app_with_two_panes();
    let focused = app.ws().focused_pane_id;

    assert!(app.handle_key_event(ctrl('w')).expect("ctrl+w"));

    assert_eq!(
        app.close_confirm,
        Some(CloseConfirm::Pane { pane_id: focused }),
        "Ctrl+W must only arm the prompt, pinning the focused pane"
    );
    assert_eq!(
        app.ws().layout.pane_count(),
        2,
        "nothing may be destroyed before the user answers"
    );
    app.shutdown();
}

#[test]
fn ctrl_w_on_a_single_pane_tab_asks_to_close_the_tab() {
    let mut app = App::new(40, 80).expect("App::new");
    app.new_tab().expect("new_tab");
    let only = app.ws().focused_pane_id;

    assert!(app.handle_key_event(ctrl('w')).expect("ctrl+w"));

    assert_eq!(
        app.close_confirm,
        Some(CloseConfirm::Tab {
            anchor_pane_id: only,
            expected_pane_ids: vec![only],
        })
    );
    assert_eq!(app.workspaces.len(), 2, "no tab closed yet");
    app.shutdown();
}

#[test]
fn ctrl_w_on_the_last_pane_of_the_only_tab_arms_nothing() {
    // Pre-#285 this was a silent no-op; it must stay one rather than
    // put up a prompt whose `y` would do nothing.
    let mut app = App::new(40, 80).expect("App::new");
    assert!(
        !app.handle_key_event(ctrl('w')).expect("ctrl+w"),
        "unhandled Ctrl+W still falls through to the PTY"
    );
    assert_eq!(app.close_confirm, None);
    app.shutdown();
}

// ─── Answering ────────────────────────────────────────────

#[test]
fn y_closes_the_confirmed_pane() {
    let mut app = app_with_two_panes();
    let right = pane_id(&app, "right");
    let left = pane_id(&app, "left");

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    assert!(app.handle_key_event(key(KeyCode::Char('y'))).expect("y"));

    assert_eq!(app.close_confirm, None, "prompt must clear after answering");
    assert!(!app.ws().panes.contains_key(&right), "target closed");
    assert!(app.ws().panes.contains_key(&left), "sibling survives");
    app.shutdown();
}

#[test]
fn uppercase_y_also_closes() {
    // Both spellings a terminal can produce for a shifted `y`: the bare
    // uppercase char, and the char with an explicit SHIFT flag. The
    // modifier allowlist must let SHIFT through while rejecting the
    // rest.
    for answer in [
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    ] {
        let mut app = app_with_two_panes();
        let right = pane_id(&app, "right");

        app.handle_key_event(ctrl('w')).expect("ctrl+w");
        app.handle_key_event(answer).expect("Y");

        assert!(
            !app.ws().panes.contains_key(&right),
            "{answer:?} must confirm"
        );
        app.shutdown();
    }
}

#[test]
fn y_closes_the_confirmed_tab() {
    let mut app = App::new(40, 80).expect("App::new");
    app.new_tab().expect("new_tab");
    let second_tab_pane = app.ws().focused_pane_id;

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    app.handle_key_event(key(KeyCode::Char('y'))).expect("y");

    assert_eq!(app.close_confirm, None);
    assert_eq!(app.workspaces.len(), 1, "tab closed");
    assert!(
        app.workspace_index_of_pane(second_tab_pane).is_none(),
        "the tab's pane is gone with it"
    );
    app.shutdown();
}

#[test]
fn n_and_esc_cancel_without_closing() {
    for answer in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
        let mut app = app_with_two_panes();
        let right = pane_id(&app, "right");

        app.handle_key_event(ctrl('w')).expect("ctrl+w");
        assert!(
            app.handle_key_event(key(answer)).expect("answer"),
            "{answer:?} must be consumed, not forwarded to the PTY"
        );

        assert_eq!(app.close_confirm, None, "{answer:?} must dismiss");
        assert!(
            app.ws().panes.contains_key(&right),
            "{answer:?} must not close anything"
        );
        app.shutdown();
    }
}

#[test]
fn unrelated_keys_are_swallowed_and_keep_the_prompt() {
    // Neither an answer nor an excuse to leak a character into the
    // pane the user is being asked about.
    let mut app = app_with_two_panes();
    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    let armed = app.close_confirm.clone();
    assert!(
        armed.is_some(),
        "prompt must be up for this test to mean anything"
    );

    for k in [
        key(KeyCode::Char('a')),
        key(KeyCode::Enter),
        key(KeyCode::Backspace),
        key(KeyCode::Up),
        ctrl('c'),
        ctrl('d'),
        ctrl('w'),
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT),
        // Under the enhanced keyboard protocol crossterm reports META
        // / HYPER / SUPER independently of ALT, so a modifier
        // allowlist — not a denylist — is what keeps these from
        // reading as consent.
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::META),
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::HYPER),
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::SUPER),
    ] {
        assert!(
            app.handle_key_event(k).expect("key"),
            "{k:?} must be consumed by the modal"
        );
        assert_eq!(app.close_confirm, armed, "{k:?} must not answer the prompt");
    }
    assert_eq!(app.ws().layout.pane_count(), 2);
    app.shutdown();
}

#[test]
fn ctrl_q_still_quits_while_confirming() {
    // The escape hatch is checked before the modal on purpose; a
    // pending prompt must never be able to trap the user.
    let mut app = app_with_two_panes();
    app.handle_key_event(ctrl('w')).expect("ctrl+w");

    assert!(app.handle_key_event(ctrl('q')).expect("ctrl+q"));
    assert!(app.should_quit, "Ctrl+Q must still quit");
    assert_eq!(app.ws().layout.pane_count(), 2, "and close nothing");
    app.shutdown();
}

#[test]
fn paste_is_swallowed_while_confirming() {
    // A bracketed paste containing "y" is not consent, and the rest of
    // the payload must not reach the PTY either.
    let mut app = app_with_two_panes();
    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    let armed = app.close_confirm.clone();

    assert!(
        app.handle_paste("y\nrm -rf /\n").expect("paste"),
        "paste must be reported as handled so it is not forwarded"
    );
    assert_eq!(app.close_confirm, armed, "paste must not answer the prompt");
    assert_eq!(app.ws().layout.pane_count(), 2);
    app.shutdown();
}

#[test]
fn mouse_is_swallowed_while_confirming() {
    // Clicks would otherwise move focus, or be forwarded to a
    // mouse-reporting TUI as control bytes — into the very pane the
    // user is being asked about.
    //
    // The coordinates have to land inside real published geometry or
    // this test proves nothing: every pointer event outside
    // `last_pane_rects` is a no-op with or without the guard. The
    // cancel-and-retry at the end is the control that proves the cell
    // is live.
    let (mut app, left, right) = two_pane_app_with_rects();
    assert_eq!(app.ws().focused_pane_id, right);
    let (col, row) = interior_of(&app, left);

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    let armed = app.close_confirm.clone();

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollDown,
    ] {
        app.handle_mouse_event(mouse_at(kind, col, row));
        assert_eq!(app.close_confirm, armed, "{kind:?} must not answer");
        assert_eq!(
            app.ws().focused_pane_id,
            right,
            "{kind:?} must not move focus off the pinned target"
        );
    }

    // Control: the same click on the same cell *does* move focus once
    // the prompt is gone. Without this, a geometry mistake above would
    // make the loop pass vacuously.
    app.handle_key_event(key(KeyCode::Char('n'))).expect("n");
    app.handle_mouse_event(mouse_at(MouseEventKind::Down(MouseButton::Left), col, row));
    assert_eq!(
        app.ws().focused_pane_id,
        left,
        "the click must be geometrically live — otherwise the loop above proves nothing"
    );
    app.shutdown();
}

#[test]
fn a_release_while_confirming_still_ends_the_in_flight_drag() {
    // Swallowing the pointer must not swallow the *release*: `Up` is
    // the only event that clears `self.dragging`, so dropping it
    // strands a drag target that hijacks the next unrelated gesture
    // (and leaves a mouse-reporting pane with an unbalanced press).
    let (mut app, _left, _right) = two_pane_app_with_rects();

    // Press on the divider between the two panes — arms a resize drag.
    app.handle_mouse_event(mouse_at(MouseEventKind::Down(MouseButton::Left), 50, 20));
    assert!(
        matches!(app.dragging, Some(DragTarget::PaneSplit(..))),
        "divider press must arm a resize drag"
    );

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    app.handle_mouse_event(mouse_at(MouseEventKind::Up(MouseButton::Left), 50, 20));

    assert!(
        app.dragging.is_none(),
        "the release must end the drag even though the modal consumed it"
    );
    assert!(
        app.close_confirm.is_some(),
        "and it must not answer the prompt"
    );
    assert_eq!(app.ws().layout.pane_count(), 2, "nothing closed or split");
    app.shutdown();
}

#[test]
fn an_armed_prompt_is_never_invisible_even_below_the_minimum_terminal_size() {
    // `render` bails out to "Terminal too small" below 40x10, before it
    // would reach the modal. Since the prompt keeps swallowing every
    // key at that size, a user resizing mid-confirmation would see an
    // app that looks wedged with no hint about `y` / `n` / `Esc`. The
    // small-terminal path has to consult the pending state too.
    let mut app = app_with_two_panes();
    assert_eq!(
        crate::ui::tiny_close_confirm_text(&app),
        None,
        "nothing to say when no prompt is pending"
    );

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    let (prompt, hint) = crate::ui::tiny_close_confirm_text(&app)
        .expect("an armed prompt must render something at any terminal size");
    assert_eq!(prompt, app.messages().close_confirm_pane);
    assert!(
        hint.contains('y') && hint.contains('n'),
        "the fallback must still name the answer keys, got {hint:?}"
    );

    app.handle_key_event(key(KeyCode::Char('n'))).expect("n");
    assert_eq!(crate::ui::tiny_close_confirm_text(&app), None);
    app.shutdown();
}

#[test]
fn the_tiny_fallback_names_the_right_target() {
    let mut app = App::new(40, 80).expect("App::new");
    app.new_tab().expect("new_tab");
    app.handle_key_event(ctrl('w')).expect("ctrl+w");

    let (prompt, _) = crate::ui::tiny_close_confirm_text(&app).expect("armed");
    assert_eq!(
        prompt,
        app.messages().close_confirm_tab,
        "a tab-close prompt must not read as a pane-close prompt"
    );
    app.shutdown();
}

// ─── The pinned target does not drift ─────────────────────

#[test]
fn y_closes_the_pinned_pane_even_after_focus_moved() {
    // Focus can move while the prompt is up (an MCP `focus_pane`, for
    // instance). `y` must still answer the question that was asked.
    let mut app = app_with_two_panes();
    let right = pane_id(&app, "right");
    let left = pane_id(&app, "left");
    assert_eq!(app.ws().focused_pane_id, right);

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    app.focus_prev_pane();
    assert_eq!(app.ws().focused_pane_id, left, "focus moved to the sibling");

    app.handle_key_event(key(KeyCode::Char('y'))).expect("y");

    assert!(
        !app.ws().panes.contains_key(&right),
        "the pane the user was asked about is the one that closes"
    );
    assert!(
        app.ws().panes.contains_key(&left),
        "the newly focused pane must survive"
    );
    app.shutdown();
}

#[test]
fn mcp_close_of_the_target_expires_the_prompt() {
    let mut app = app_with_two_panes();
    let right = pane_id(&app, "right");

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    app.handle_close(&ipc::PaneRef::Id(right), None)
        .expect("mcp close of the confirmation target");

    assert_eq!(
        app.close_confirm, None,
        "a prompt whose target vanished must expire, not retarget"
    );
    app.shutdown();
}

#[test]
fn pane_prompt_expires_rather_than_escalating_to_a_tab_close() {
    // The user agreed to "close this pane", never to "close this tab".
    // If an MCP close leaves the target as its tab's only pane, the
    // prompt must expire instead of being silently promoted.
    let mut app = app_with_two_panes();
    let right = pane_id(&app, "right");
    let left = pane_id(&app, "left");
    app.new_tab()
        .expect("second tab so a tab close would be legal");
    app.active_tab = 0;
    app.ws_mut().focused_pane_id = right;

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    assert_eq!(
        app.close_confirm,
        Some(CloseConfirm::Pane { pane_id: right })
    );

    app.handle_close(&ipc::PaneRef::Id(left), None)
        .expect("mcp closes the sibling");

    assert_eq!(app.close_confirm, None, "prompt must expire");
    assert!(app.ws().panes.contains_key(&right), "target still alive");
    assert_eq!(app.workspaces.len(), 2, "no tab was closed");
    app.shutdown();
}

#[test]
fn tab_prompt_expires_when_a_split_grows_the_tab() {
    // Confirming after an MCP split would destroy a pane the user
    // never saw, so the pane-id snapshot mismatch cancels instead.
    let mut app = App::new(40, 80).expect("App::new");
    app.new_tab().expect("new_tab");

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    assert!(matches!(app.close_confirm, Some(CloseConfirm::Tab { .. })));

    app.handle_split(
        &ipc::PaneRef::Focused,
        ipc::Direction::Vertical,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("mcp split");

    assert_eq!(
        app.close_confirm, None,
        "the tab the user agreed to close no longer exists as asked"
    );
    assert_eq!(app.workspaces.len(), 2, "no tab closed");
    app.shutdown();
}

#[test]
fn y_after_a_natural_exit_removes_the_pane_without_double_emitting() {
    // A pane whose shell exited stays in the layout with `exited =
    // true`, so the prompt remains answerable; `y` is then just a
    // layout removal. `exit_event_emitted` (already set by the EOF
    // path) must keep `PaneExited` exactly-once.
    let mut app = app_with_two_panes();
    let right = pane_id(&app, "right");

    // Simulate what `drain_pty_events` does on PtyEof.
    {
        let pane = app.workspaces[0].panes.get_mut(&right).expect("right pane");
        pane.exited = true;
        pane.exit_event_emitted = true;
    }

    let (_sub_id, rx) = app.event_bus.subscribe();

    app.handle_key_event(ctrl('w')).expect("ctrl+w");
    app.handle_key_event(key(KeyCode::Char('y'))).expect("y");

    assert!(
        !app.ws().panes.contains_key(&right),
        "y must still remove an exited pane from the layout"
    );
    let mut saw_right_exited = false;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PaneExited { id, .. } = ev {
            if id == right {
                saw_right_exited = true;
            }
        }
    }
    assert!(
        !saw_right_exited,
        "PaneExited must not fire twice for a naturally exited pane"
    );
    app.shutdown();
}

// ─── The MCP path stays immediate ─────────────────────────

#[test]
fn mcp_close_never_arms_a_confirmation() {
    // Automation must not block on a human keystroke: `close_pane`
    // closes, full stop.
    let mut app = app_with_two_panes();
    let right = pane_id(&app, "right");

    let closed = app
        .handle_close(&ipc::PaneRef::Id(right), None)
        .expect("mcp close");

    assert_eq!(closed, right);
    assert_eq!(app.close_confirm, None, "no prompt may be armed");
    assert!(!app.ws().panes.contains_key(&right), "closed immediately");
    app.shutdown();
}

#[test]
fn mcp_close_of_a_single_pane_tab_closes_the_tab_immediately() {
    let mut app = App::new(40, 80).expect("App::new");
    app.new_tab().expect("new_tab");
    let second = app.ws().focused_pane_id;

    app.handle_close(&ipc::PaneRef::Id(second), None)
        .expect("mcp close");

    assert_eq!(app.close_confirm, None);
    assert_eq!(app.workspaces.len(), 1, "tab closed without asking");
    app.shutdown();
}
