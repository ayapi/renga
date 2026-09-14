//! Tab-visibility gating of `dirty` in `drain_pty_events` (renga-pgd).
//!
//! Raw `PtyOutput` only repaints when its pane is on the active tab;
//! background-tab spinners must not force full-fps repaints of a
//! screen they don't appear on. State-changing events (PtyEof,
//! CwdChanged) punch through regardless of tab, exactly like they
//! punch through the IME freeze gate.

use std::time::{Duration, Instant};

use crate::app::{frame_diagnostics, App, AppEvent};

/// Drain until the freshly spawned shells stop emitting startup
/// output, so live PtyOutput can't race the synthetic events these
/// tests inject. Requires several consecutive quiet windows — a
/// single one can fall inside a >50ms pause mid shell startup (cold
/// CI machine, profile scripts) and let a late prompt chunk land
/// after the test cleared `dirty`. Caps at ~6s.
fn quiesce(app: &mut App) {
    let mut quiet_windows = 0;
    for _ in 0..120 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if app.drain_pty_events() {
            quiet_windows = 0;
        } else {
            quiet_windows += 1;
            if quiet_windows >= 5 {
                return;
            }
        }
    }
}

#[test]
fn pty_output_from_active_tab_pane_dirties() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyOutput(pane_a, 1))
        .expect("send PtyOutput");
    app.drain_pty_events();

    assert!(app.dirty, "visible pane output must repaint");
}

#[test]
fn pty_output_from_background_tab_pane_does_not_dirty() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_tab0 = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    assert_eq!(app.active_tab, 1, "new tab becomes active");
    quiesce(&mut app);
    app.dirty = false;

    // Output for the now-hidden tab-0 pane.
    app.event_tx
        .send(AppEvent::PtyOutput(pane_tab0, 1))
        .expect("send PtyOutput");
    app.drain_pty_events();

    assert!(
        !app.dirty,
        "background-tab output must not repaint the visible tab"
    );
    app.shutdown();
}

#[test]
fn expired_erase_hold_flushes_for_background_pane_and_records_bytes() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_tab0 = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    assert_eq!(app.active_tab, 1, "new tab becomes active");

    let started = Instant::now();
    let event_tx = app.event_tx.clone();
    let pane = app.workspaces[0]
        .panes
        .get(&pane_tab0)
        .expect("background pane");
    pane.process_test_output_at(b"before", started, &event_tx);
    app.drain_pty_events_at(started);

    let held = b"\x1b[2J\x1b[Hafter";
    let pane = app.workspaces[0]
        .panes
        .get(&pane_tab0)
        .expect("background pane");
    pane.process_test_output_at(held, started, &event_tx);
    assert!(pane
        .parser
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .screen()
        .contents()
        .starts_with("before"));

    app.dirty = false;
    frame_diagnostics::begin_test_frame(started);
    assert!(app.drain_pty_events_at(started + Duration::from_millis(40)));

    let pane = app.workspaces[0]
        .panes
        .get(&pane_tab0)
        .expect("background pane");
    assert!(pane
        .parser
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .screen()
        .contents()
        .starts_with("after"));
    assert_eq!(
        frame_diagnostics::output_bytes_for_test(pane_tab0),
        held.len()
    );
    assert!(!app.dirty, "background output must retain dirty gating");
    frame_diagnostics::clear_test_frame();
    app.shutdown();
}

#[test]
fn pty_output_for_unknown_pane_does_not_dirty() {
    // A reader thread can still drain a chunk after its pane was
    // removed from every workspace; that must not repaint either.
    let mut app = App::new(40, 80).expect("App::new");
    quiesce(&mut app);
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyOutput(usize::MAX, 1))
        .expect("send PtyOutput");
    app.drain_pty_events();

    assert!(!app.dirty, "output for a vanished pane must not repaint");
}

#[test]
fn pty_eof_from_background_tab_still_dirties() {
    // Pane exit changes chrome the user can see from anywhere (the
    // exactly-once PaneExited event, future exit badges), so it keeps
    // repainting regardless of which tab the pane lives on.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_tab0 = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyEof(pane_tab0))
        .expect("send PtyEof");
    app.drain_pty_events();

    assert!(app.dirty, "state changes punch through tab gating");
    app.shutdown();
}

#[test]
fn mixed_batch_with_background_output_and_state_change_dirties() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_tab0 = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyOutput(pane_tab0, 1))
        .expect("send PtyOutput");
    app.event_tx
        .send(AppEvent::PtyEof(pane_tab0))
        .expect("send PtyEof");
    app.drain_pty_events();

    assert!(app.dirty, "any state change in the batch must repaint");
    app.shutdown();
}
