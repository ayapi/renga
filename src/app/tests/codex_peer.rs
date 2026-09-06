use super::super::*;
use crate::app::codex_peer::{
    codex_composer_has_draft_on_screen, normalized_codex_composer_text,
    CODEX_PEER_DRAFT_STALL_TIMEOUT, CODEX_PEER_NUDGE_MAX_RETRIES,
};

fn seed_focused_pane_screen(app: &mut App, bytes: &[u8]) -> usize {
    let pane_id = app.ws().focused_pane_id;
    seed_pane_screen(app, pane_id, bytes);
    pane_id
}

fn seed_pane_screen(app: &mut App, pane_id: usize, bytes: &[u8]) {
    let pane = app.ws_mut().panes.get_mut(&pane_id).expect("pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.process(bytes);
}

fn seed_codex_draft(app: &mut App, pane_id: usize) {
    seed_pane_screen(
        app,
        pane_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA typed draft\x1b[1;15H",
    );
}

fn seed_codex_live_ready_placeholder(app: &mut App, pane_id: usize) {
    seed_pane_screen(
        app,
        pane_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\r\n\r\n  gpt-5.6-sol medium \xC2\xB7 cwd\x1b[1;3H",
    );
}

fn seed_codex_busy_placeholder(app: &mut App, pane_id: usize) {
    seed_pane_screen(
        app,
        pane_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (1m 03s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA \x1b[2mImprove documentation in @filename\x1b[22m\x1b[6;1H  gpt-5.6-sol high \xC2\xB7 cwd\x1b[6;20H",
    );
}

fn seed_codex_busy_composer(app: &mut App, pane_id: usize, text: &str) {
    let screen = format!(
        "\x1b[?25h\x1b[2J\x1b[3;1H\u{25e6} Working (1m 03s \u{2022} esc to interrupt)\x1b[6;1H\u{203a} {text}\x1b[10;1H  tab to queue message  51% context left\x1b[8;20H"
    );
    seed_pane_screen(app, pane_id, screen.as_bytes());
}

fn seed_codex_long_busy_composer(app: &mut App, pane_id: usize, text: &str) {
    let chars = text.chars().collect::<Vec<_>>();
    let mut screen =
        String::from("\x1b[?25h\x1b[2J\x1b[H\u{25e6} Working (1m 03s \u{2022} esc to interrupt)");
    for (index, chunk) in chars.chunks(20).enumerate() {
        let row = 4 + index;
        let text = chunk.iter().collect::<String>();
        let prefix = if index == 0 { "\u{203a} " } else { "  " };
        screen.push_str(&format!("\x1b[{row};1H{prefix}{text}"));
    }
    let blank_row = 4 + chars.chunks(20).len();
    let footer_row = blank_row + 1;
    let cursor_row = blank_row - 1;
    let cursor_col = chars.chunks(20).last().map_or(3, |chunk| chunk.len() + 3);
    screen.push_str(&format!(
        "\x1b[{footer_row};1H  tab to queue message  51% context left\x1b[{cursor_row};{cursor_col}H"
    ));
    seed_pane_screen(app, pane_id, screen.as_bytes());
}

fn seed_codex_idle_composer(app: &mut App, pane_id: usize, text: &str) {
    let chars = text.chars().collect::<Vec<_>>();
    let mut screen = String::from("\x1b[?25h\x1b[2J");
    for (index, chunk) in chars.chunks(20).enumerate() {
        let row = 1 + index;
        let text = chunk.iter().collect::<String>();
        let prefix = if index == 0 { "\u{203a} " } else { "  " };
        screen.push_str(&format!("\x1b[{row};1H{prefix}{text}"));
    }
    let blank_row = 1 + chars.chunks(20).len();
    let footer_row = blank_row + 1;
    let cursor_row = blank_row - 1;
    let cursor_col = chars.chunks(20).last().map_or(3, |chunk| chunk.len() + 3);
    screen.push_str(&format!(
        "\x1b[{footer_row};1H  gpt-5.6-sol medium \u{b7} cwd\x1b[{cursor_row};{cursor_col}H"
    ));
    seed_pane_screen(app, pane_id, screen.as_bytes());
}

fn seed_expected_codex_peer_composer(app: &mut App, pane_id: usize, from_pane: usize) {
    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane,
        from_name: None,
        from_kind: None,
    });
    seed_codex_idle_composer(app, pane_id, &expected);
}

fn make_codex_native_queue_ready(app: &mut App, pane_id: usize) {
    let queue = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .expect("pending nudge");
    match queue.front_mut().expect("pending delivery") {
        PendingCodexPeerDelivery::QueueAt { ready_at, .. } => *ready_at = Instant::now(),
        other => panic!("expected native queue stage, got {other:?}"),
    }
}

fn make_codex_submit_ready(app: &mut App, pane_id: usize) {
    let queue = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .expect("pending nudge");
    match queue.front_mut().expect("pending delivery") {
        PendingCodexPeerDelivery::SubmitAt { ready_at, .. } => *ready_at = Instant::now(),
        other => panic!("expected submit stage, got {other:?}"),
    }
}

fn expire_codex_native_queue(app: &mut App, pane_id: usize) {
    let queue = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .expect("pending nudge");
    match queue.front_mut().expect("pending delivery") {
        PendingCodexPeerDelivery::QueueAt {
            ready_at,
            expires_at,
            ..
        } => {
            *ready_at = Instant::now();
            *expires_at = Instant::now();
        }
        other => panic!("expected native queue stage, got {other:?}"),
    }
}

fn setup_unfocused_registered_codex() -> (App, usize, usize) {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    (app, sender_id, codex_id)
}

#[test]
fn codex_peer_delivery_ready_accepts_ready_for_input_fallback() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[Hready for input");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(App::codex_peer_delivery_ready(true, pane));

    app.shutdown();
}

#[test]
fn codex_peer_delivery_ready_rejects_queue_banner() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[HTab to queue message");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(!App::codex_peer_delivery_ready(true, pane));

    app.shutdown();
}

#[test]
fn codex_peer_delivery_ready_rejects_busy_codex_prompt() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id =
        seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[H\xE2\x80\xBA typed draft\x1b[1;14H");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(!App::codex_peer_delivery_ready(true, pane));

    app.shutdown();
}

#[test]
fn codex_peer_delivery_ready_requires_codex_registration_or_process() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[Hready for input");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(!App::codex_peer_delivery_ready(false, pane));

    app.shutdown();
}

#[test]
fn format_codex_peer_message_includes_sender_and_check_messages_guidance() {
    let formatted = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: 7,
        from_name: Some("planner".to_string()),
        from_kind: Some(PeerClientKind::Claude),
    });
    assert!(!formatted.contains('\n'), "{formatted:?}");
    assert!(
        formatted.contains("Peer request from id=7 name=planner kind=claude."),
        "{formatted:?}"
    );
    assert!(
        formatted.contains("Run check_messages now."),
        "{formatted:?}"
    );
    assert!(
        formatted.contains("use send_message only when a reply or status update is needed."),
        "{formatted:?}"
    );
}

#[test]
fn handle_peer_send_emits_peer_inbox_to_sibling_in_same_tab() {
    // Split pane A's workspace to create a sibling. Sending from
    // A to the sibling should emit Event::PeerInbox carrying the
    // sender id, the body, and a stable timestamp. This is the
    // core happy-path of #97: without this event the MCP peer
    // subprocess has nothing to push as a channel notification.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_register_client(sibling_id, PeerClientKind::Claude)
        .expect("peer registration");
    app.handle_peer_set_ready(sibling_id, PeerClientKind::Claude, true)
        .expect("peer readiness");
    // Drain PaneStarted events from the split so the assertion below
    // only sees the PeerInbox we care about.
    while let Ok(ev) = rx.try_recv() {
        if !matches!(ev, ipc::Event::PaneStarted { .. }) {
            panic!("unexpected event before peer send: {ev:?}");
        }
    }
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello sibling".to_string(),
    )
    .expect("peer send");
    let mut found = false;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox {
            target_pane,
            from_pane,
            body,
            ..
        } = ev
        {
            assert_eq!(target_pane, sibling_id);
            assert_eq!(from_pane, sender_id);
            assert_eq!(body, "hello sibling");
            found = true;
            break;
        }
    }
    assert!(found, "expected PeerInbox event after handle_peer_send");
    app.shutdown();
}

#[test]
fn handle_peer_send_waits_for_target_peer_registration() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "queued before registration".to_string(),
    )
    .expect("peer send");

    assert!(
        rx.try_iter()
            .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })),
        "an unregistered target has no subscriber yet"
    );

    app.handle_peer_register_client(sibling_id, PeerClientKind::Codex)
        .expect("peer registration");

    assert!(
        rx.try_iter()
            .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })),
        "kind metadata alone must not flush before the subscriber is ready"
    );
    app.handle_peer_set_ready(sibling_id, PeerClientKind::Codex, true)
        .expect("peer readiness");

    assert!(!app.pending_codex_peer_messages.contains_key(&sibling_id));

    let event = rx
        .try_iter()
        .find(|event| matches!(event, ipc::Event::PeerInbox { .. }))
        .expect("registration should release the queued message");
    let delivery_id = match event {
        ipc::Event::PeerInbox {
            delivery_id: Some(delivery_id),
            target_pane,
            from_pane,
            body,
            ..
        } => {
            assert_eq!(target_pane, sibling_id);
            assert_eq!(from_pane, sender_id);
            assert_eq!(body, "queued before registration");
            delivery_id
        }
        other => panic!("unexpected event: {other:?}"),
    };
    app.handle_peer_inbox_ack(sibling_id, delivery_id)
        .expect("MCP retained queued message");
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn handle_peer_send_refuses_when_pre_registration_queue_is_full() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");

    for n in 0..crate::app::codex_peer::PENDING_PEER_INBOX_MAX_MESSAGES {
        assert_eq!(
            app.handle_peer_send(
                sender_id,
                &ipc::PaneRef::Id(sibling_id),
                format!("queued-{n}"),
            )
            .expect("queue has capacity"),
            ipc::PeerSendOutcome::Queued
        );
    }
    let err = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "one-too-many".to_string(),
        )
        .expect_err("bounded queue must refuse overflow");
    assert_eq!(err.code, Some(ipc::err_code::PEER_QUEUE_FULL));

    // A rejected send must not enter the dedupe window. Retrying the
    // same body still has to report queue-full, never false success.
    let retry = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "one-too-many".to_string(),
        )
        .expect_err("rejected body must remain retryable");
    assert_eq!(retry.code, Some(ipc::err_code::PEER_QUEUE_FULL));
    app.shutdown();
}

#[test]
fn pre_registration_queue_flushes_in_fifo_order() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    while rx.try_recv().is_ok() {}

    for body in ["first", "second", "third"] {
        app.handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), body.to_string())
            .expect("queued send");
    }
    app.handle_peer_register_client(sibling_id, PeerClientKind::Codex)
        .expect("registration");
    app.handle_peer_set_ready(sibling_id, PeerClientKind::Codex, true)
        .expect("readiness");

    let bodies: Vec<String> = rx
        .try_iter()
        .filter_map(|event| match event {
            ipc::Event::PeerInbox { body, .. } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(bodies, ["first", "second", "third"]);
    app.shutdown();
}

#[test]
fn codex_readiness_sets_kind_and_rearms_nudge_atomically() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("focus sender");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "queued for codex".to_string(),
    )
    .expect("queued send");
    assert!(!app.peer_client_kinds.contains_key(&sibling_id));

    app.handle_peer_set_ready(sibling_id, PeerClientKind::Codex, true)
        .expect("atomic readiness");

    assert_eq!(
        app.peer_client_kinds.get(&sibling_id),
        Some(&PeerClientKind::Codex)
    );
    assert!(!app.pending_codex_peer_messages.contains_key(&sibling_id));
    let delivery_id = *app.pending_peer_deliveries.keys().next().unwrap();
    app.handle_peer_inbox_ack(sibling_id, delivery_id)
        .expect("MCP retained queued message");
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn closing_pane_discards_its_pre_registration_queue() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "discard me".to_string(),
    )
    .expect("queued send");
    assert!(app.pending_peer_inbox.contains_key(&sibling_id));

    app.handle_close(&ipc::PaneRef::Id(sibling_id))
        .expect("close sibling");
    assert!(!app.pending_peer_inbox.contains_key(&sibling_id));
    app.shutdown();
}

#[test]
fn shutdown_discards_all_pre_registration_queues() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "discard on shutdown".to_string(),
    )
    .expect("queued send");
    app.shutdown();
    assert!(app.pending_peer_inbox.is_empty());
}

#[test]
fn closing_tab_discards_its_pre_registration_queues() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.new_tab().expect("second tab");
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "discard with tab".to_string(),
    )
    .expect("queued send");

    let closing_tab = app.active_tab;
    app.close_tab(closing_tab);
    assert!(!app.pending_peer_inbox.contains_key(&sibling_id));
    app.shutdown();
}

#[test]
fn handle_peer_send_loops_back_to_sender_pane() {
    // Regression for renga#215: when the resolved target is the
    // sender pane itself (e.g. claude-org-ja's peer_notify resolving
    // "secretary" from a shell inside the secretary pane), the
    // handler must still emit PeerInbox so the local check_messages
    // loop picks it up. Prior to the fix the self-send was silently
    // dropped while JSON-RPC reported Delivered.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    app.handle_peer_register_client(sender_id, PeerClientKind::Claude)
        .expect("peer registration");
    app.handle_peer_set_ready(sender_id, PeerClientKind::Claude, true)
        .expect("peer readiness");
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sender_id),
        "self ping".to_string(),
    )
    .expect("self send");

    let mut found = false;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox {
            target_pane,
            from_pane,
            body,
            ..
        } = ev
        {
            assert_eq!(target_pane, sender_id);
            assert_eq!(from_pane, sender_id);
            assert_eq!(body, "self ping");
            found = true;
            break;
        }
    }
    assert!(found, "expected PeerInbox event for self-send (renga#215)");
    app.shutdown();
}

fn assert_peer_send_undeliverable_without_queued_state(
    app: &mut App,
    sender_id: usize,
    target: &ipc::PaneRef,
) -> Vec<u8> {
    let (_sub_id, rx) = app.event_bus.subscribe();
    while rx.try_recv().is_ok() {}
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(sender_id, target, "must not be queued".into(), reply_tx);
    let outcome = reply_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("immediate peer-send reply")
        .expect("undeliverable is a success-shaped outcome");
    assert_eq!(outcome, ipc::PeerSendOutcome::Undeliverable);
    assert!(app.pending_peer_inbox.is_empty());
    assert!(app.pending_peer_deliveries.is_empty());
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })));
    // Mirrors the peer-send response shape in ipc/server.rs.
    let response = serde_json::to_vec(&serde_json::json!({ "delivery": outcome }))
        .expect("serialize peer-send response");
    assert_eq!(response, br#"{"delivery":"undeliverable"}"#);
    response
}

#[test]
fn peer_send_returns_undeliverable_for_unknown_id_without_queued_state() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let target = ipc::PaneRef::Id(usize::MAX);

    assert_peer_send_undeliverable_without_queued_state(&mut app, sender_id, &target);
    app.shutdown();
}

#[test]
fn peer_send_returns_undeliverable_for_unknown_name_without_queued_state() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let target = ipc::PaneRef::Name("missing-peer".into());

    assert_peer_send_undeliverable_without_queued_state(&mut app, sender_id, &target);
    app.shutdown();
}

#[test]
fn peer_send_returns_undeliverable_for_cross_tab_id_without_queued_state() {
    // Cross-tab and unresolved targets use the same response so callers
    // cannot discover panes in other tabs by probing ids.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    // Open a fresh tab; its pane id is distinct from sender's.
    let other_tab_pane = app
        .handle_new_tab(None, None, None, None, None)
        .expect("new tab succeeds")
        .id;
    assert_ne!(
        other_tab_pane, sender_id,
        "new_tab must allocate a fresh pane id"
    );
    let unresolved_response = assert_peer_send_undeliverable_without_queued_state(
        &mut app,
        sender_id,
        &ipc::PaneRef::Id(usize::MAX),
    );
    let cross_tab_response = assert_peer_send_undeliverable_without_queued_state(
        &mut app,
        sender_id,
        &ipc::PaneRef::Id(other_tab_pane),
    );
    assert_eq!(cross_tab_response, unresolved_response);
    app.shutdown();
}

#[test]
fn peer_send_returns_undeliverable_for_cross_tab_name_without_queued_state() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let other_tab_pane = app
        .handle_new_tab(None, Some("other-peer".into()), None, None, None)
        .expect("new tab succeeds")
        .id;
    assert_ne!(other_tab_pane, sender_id);
    let target = ipc::PaneRef::Name("other-peer".into());

    assert_peer_send_undeliverable_without_queued_state(&mut app, sender_id, &target);
    app.shutdown();
}

#[test]
fn peer_send_from_inactive_tab_resolves_duplicate_name_in_sender_tab() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sender_tab_target = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("worker".into()),
            None,
            None,
        )
        .expect("split sender tab");
    app.handle_peer_register_client(sender_tab_target, PeerClientKind::Claude)
        .expect("register sender-tab target");
    app.handle_peer_set_ready(sender_tab_target, PeerClientKind::Claude, true)
        .expect("ready sender-tab target");

    let active_tab_target = app
        .handle_new_tab(None, Some("worker".into()), None, None, None)
        .expect("new active tab with duplicate name")
        .id;
    assert_ne!(active_tab_target, sender_tab_target);
    let (_sub_id, rx) = app.event_bus.subscribe();

    let outcome = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Name("worker".into()),
            "route within sender tab".into(),
        )
        .expect("peer send");
    assert_eq!(outcome, ipc::PeerSendOutcome::Delivered);
    let target_pane = rx
        .try_iter()
        .find_map(|event| match event {
            ipc::Event::PeerInbox { target_pane, .. } => Some(target_pane),
            _ => None,
        })
        .expect("PeerInbox for sender-tab target");
    assert_eq!(target_pane, sender_tab_target);
    assert_ne!(target_pane, active_tab_target);
    app.shutdown();
}

#[test]
fn peer_send_from_inactive_tab_joins_sender_tabs_duplicate_name_delivery() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sender_tab_target = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("worker".into()),
            None,
            None,
        )
        .expect("split sender tab");
    app.handle_peer_register_client(sender_tab_target, PeerClientKind::Claude)
        .expect("register sender-tab target");
    app.handle_peer_set_ready(sender_tab_target, PeerClientKind::Claude, true)
        .expect("ready sender-tab target");
    let active_tab_target = app
        .handle_new_tab(None, Some("worker".into()), None, None, None)
        .expect("new active tab with duplicate name")
        .id;
    assert_ne!(active_tab_target, sender_tab_target);
    let (_sub_id, rx) = app.event_bus.subscribe();
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, second_rx) = oneshot::channel();
    let target = ipc::PaneRef::Name("worker".into());

    app.begin_peer_send(sender_id, &target, "same body".into(), first_tx);
    app.begin_peer_send(sender_id, &target, "same body".into(), second_tx);

    assert_eq!(
        app.pending_peer_deliveries.len(),
        1,
        "identical in-flight sends must join one delivery (renga-bcb)"
    );
    let delivery_id = peer_delivery_id(&rx);
    assert_eq!(
        app.pending_peer_deliveries[&delivery_id].target_pane,
        sender_tab_target
    );
    assert_eq!(app.pending_peer_deliveries[&delivery_id].replies.len(), 2);
    app.handle_peer_inbox_ack(sender_tab_target, delivery_id)
        .expect("receipt");
    for reply in [first_rx, second_rx] {
        assert_eq!(
            reply.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
            ipc::PeerSendOutcome::Delivered
        );
    }
    app.shutdown();
}

#[test]
fn handle_peer_send_queues_codex_nudge_and_emits_peer_inbox() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");

    let peer_inbox = rx
        .try_iter()
        .find(|event| matches!(event, ipc::Event::PeerInbox { .. }))
        .expect("Codex delivery should still emit PeerInbox");
    match peer_inbox {
        ipc::Event::PeerInbox {
            target_pane,
            from_pane,
            from_name,
            from_kind,
            body,
            ..
        } => {
            assert_eq!(target_pane, sibling_id);
            assert_eq!(from_pane, sender_id);
            assert_eq!(from_name.as_deref(), None);
            assert_eq!(from_kind, None);
            assert_eq!(body, "hello codex");
        }
        other => panic!("unexpected event: {other:?}"),
    }
    let queued = app
        .pending_codex_peer_messages
        .get(&sibling_id)
        .expect("queued codex peer message");
    assert_eq!(queued.len(), 1);
    match &queued[0] {
        PendingCodexPeerDelivery::Draft { message: msg, .. } => {
            assert_eq!(msg.from_pane, sender_id);
            assert_eq!(msg.from_name.as_deref(), None);
            assert_eq!(msg.from_kind, None);
        }
        other => panic!("unexpected queued delivery: {other:?}"),
    }
    app.shutdown();
}

#[test]
fn handle_peer_send_coalesces_codex_nudges_per_pane() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("first peer send");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello again codex".to_string(),
    )
    .expect("second peer send");

    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "multiple queued inbox messages should share a single pane-local nudge"
    );
    assert_eq!(
        rx.try_iter()
            .filter(|event| matches!(event, ipc::Event::PeerInbox { .. }))
            .count(),
        2,
        "a new arrival behind an unread head must still emit its normal inbox delivery event"
    );
    app.shutdown();
}
#[test]
fn pane_expects_codex_peer_delivery_accepts_registered_codex_without_title() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
        *pane.title.lock().unwrap() = String::new();
    }
    assert!(
        !app.pane_expects_codex_peer_delivery(app.active_tab, pane_id),
        "blank title with no registration should not look like Codex"
    );

    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    assert!(
        app.pane_expects_codex_peer_delivery(app.active_tab, pane_id),
        "registered Codex peer must count even when OSC title detection never fired"
    );
    app.shutdown();
}

#[test]
fn pane_expects_codex_peer_delivery_accepts_pending_codex_startup() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
        *pane.title.lock().unwrap() = String::new();
        pane.pending_startup = Some(b"codex --model gpt-5\n".to_vec());
    }

    assert!(
        app.pane_expects_codex_peer_delivery(app.active_tab, pane_id),
        "queued codex startup should count before MCP registration lands"
    );
    app.shutdown();
}

#[test]
fn forward_key_to_pty_clears_codex_transcript_overlay_hint() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists")
        .set_codex_transcript_overlay_hint_for_test(true);

    app.forward_key_to_pty(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("forward key");

    assert!(
        !app.ws()
            .panes
            .get(&pane_id)
            .expect("focused pane exists")
            .codex_transcript_overlay_hint_for_test(),
        "direct PTY key forwarding must clear transcript fallback state"
    );
    app.shutdown();
}

#[test]
fn forward_paste_to_pty_clears_codex_transcript_overlay_hint() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists")
        .set_codex_transcript_overlay_hint_for_test(true);

    app.forward_paste_to_pty("hello").expect("forward paste");

    assert!(
        !app.ws()
            .panes
            .get(&pane_id)
            .expect("focused pane exists")
            .codex_transcript_overlay_hint_for_test(),
        "direct PTY paste forwarding must clear transcript fallback state"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_defers_codex_nudge_while_target_is_focused() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");
    seed_codex_draft(&mut app, sibling_id);
    while rx.try_recv().is_ok() {}

    let outcome = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "hello focused codex".to_string(),
        )
        .expect("peer send");

    let peer_inbox = rx
        .try_iter()
        .find(|event| matches!(event, ipc::Event::PeerInbox { .. }))
        .expect("focused Codex should still receive PeerInbox");
    match peer_inbox {
        ipc::Event::PeerInbox {
            target_pane, body, ..
        } => {
            assert_eq!(target_pane, sibling_id);
            assert_eq!(body, "hello focused codex");
        }
        other => panic!("unexpected event: {other:?}"),
    }
    let notification = app
        .visible_codex_peer_notification()
        .expect("focused Codex target should show a notification overlay");
    assert_eq!(notification.target_pane, sibling_id);
    assert_eq!(notification.pending_count, 1);
    assert_eq!(
        outcome,
        ipc::PeerSendOutcome::PendingUserConfirmation,
        "the reported outcome must match the notification that actually retains the nudge"
    );
    let duplicate = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "hello focused codex".to_string(),
        )
        .expect("duplicate peer send");
    assert_eq!(
        duplicate,
        ipc::PeerSendOutcome::PendingUserConfirmation,
        "dedupe must replay the original pending outcome"
    );
    assert_eq!(
        app.visible_codex_peer_notification()
            .expect("dedupe must preserve the retained notification")
            .pending_count,
        1,
        "dedupe must not manufacture a second retained nudge"
    );
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused Codex target should not queue an immediate PTY nudge"
    );

    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused Codex target should stay notification-only while it remains focused"
    );

    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    app.flush_pending_codex_peer_messages();
    assert!(
        app.visible_codex_peer_notification().is_none(),
        "moving focus away should hand the notification back to the worker queue"
    );
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "unfocused Codex target should regain a queued nudge"
    );
    seed_codex_live_ready_placeholder(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "first unfocused flush should advance the deferred nudge to submit stage"
    );
    seed_expected_codex_peer_composer(&mut app, sibling_id, sender_id);
    make_codex_submit_ready(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "second unfocused flush should submit the deferred nudge"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_coalesces_focused_codex_notifications() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");
    seed_codex_draft(&mut app, sibling_id);

    let first = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "hello focused codex".to_string(),
        )
        .expect("first peer send");
    let second = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "hello again focused codex".to_string(),
        )
        .expect("second peer send");

    let notification = app
        .visible_codex_peer_notification()
        .expect("focused Codex target should still show one notification");
    assert_eq!(notification.pending_count, 2);
    assert_eq!(first, ipc::PeerSendOutcome::PendingUserConfirmation);
    assert_eq!(second, ipc::PeerSendOutcome::PendingUserConfirmation);
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused notifications should not leak into the PTY nudge queue"
    );
    app.shutdown();
}

#[test]
fn focused_codex_notification_esc_dismisses_without_queueing_nudge() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");
    seed_codex_draft(&mut app, sibling_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let consumed = app.handle_key_event(esc).expect("dismiss notification");
    assert!(consumed);
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "dismissing the notification should not silently queue a PTY nudge"
    );
    app.shutdown();
}

#[test]
fn focused_codex_notification_accept_with_empty_composer_submits_nudge() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");
    seed_codex_draft(&mut app, sibling_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");

    seed_codex_live_ready_placeholder(&mut app, sibling_id);
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();

    let commit = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    let consumed = app.handle_key_event(commit).expect("commit notification");
    assert!(consumed);
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));

    seed_expected_codex_peer_composer(&mut app, sibling_id, sender_id);
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();
    make_codex_submit_ready(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();

    assert_eq!(
        app.ws().panes.get(&sibling_id).expect("pane").test_input(),
        b"\r"
    );
    assert!(!app.pending_codex_peer_messages.contains_key(&sibling_id));
    app.shutdown();
}

#[test]
fn focused_codex_notification_accept_refuses_existing_draft() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");
    seed_codex_draft(&mut app, sibling_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "do not send my draft".to_string(),
    )
    .expect("peer send");
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();

    let commit = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    let consumed = app.handle_key_event(commit).expect("refuse notification");

    assert!(consumed);
    assert!(app.visible_codex_peer_notification().is_some());
    assert!(app
        .ws()
        .panes
        .get(&sibling_id)
        .expect("pane")
        .test_input()
        .is_empty());
    assert!(!app.pending_codex_peer_messages.contains_key(&sibling_id));
    app.shutdown();
}

#[test]
fn flush_pending_codex_peer_messages_requires_ready_screen() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        *pane.title.lock().unwrap() = "Codex".to_string();
        let mut parser = pane.parser.lock().unwrap();
        parser.process(b"\x1b[?25lworking");
    }
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "busy Codex pane should keep the message queued"
    );

    seed_codex_live_ready_placeholder(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "first ready flush should advance to submit stage"
    );
    seed_expected_codex_peer_composer(&mut app, sibling_id, sender_id);
    make_codex_submit_ready(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "second flush should submit the queued nudge"
    );
    app.shutdown();
}

#[test]
fn flush_pending_codex_peer_messages_waits_for_non_blank_codex_screen() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");

    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "blank Codex screen should keep the nudge queued"
    );

    seed_codex_live_ready_placeholder(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "non-blank Codex screen should advance to submit stage first"
    );
    seed_expected_codex_peer_composer(&mut app, sibling_id, sender_id);
    make_codex_submit_ready(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "second flush should submit the queued nudge"
    );
    app.shutdown();
}

#[test]
fn codex_prompt_allows_peer_nudge_uses_recent_content_on_tall_screens() {
    let mut parser = vt100::Parser::new(120, 120, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\
          Tip: NEW: JavaScript REPL is now available in /experimental.\n\
          \n\
          \n\
          \xE2\x80\xBA Summarize recent commits\n\
          \n\
            gpt-5.4 high \xC2\xB7 cwd\x1b[4;3H",
    );

    let screen = parser.screen();
    let tail = screen_tail_lines(screen).join("\n").to_ascii_lowercase();
    assert!(
        tail.contains("summarize recent commits"),
        "tail snapshot should stay anchored to the recent Codex prompt"
    );
    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(screen),
        Some(true),
        "recent Codex prompt on a tall screen should allow the peer nudge"
    );
}

#[test]
fn transcript_prompt_before_unknown_modal_does_not_accept_nudge() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (12s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA old request\x1b[5;1Htool output\x1b[7;1HAllow this command?\x1b[8;1HYes / No\x1b[4;3H",
    );
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(codex_composer_has_draft_on_screen(parser.screen()), None);
        assert_eq!(normalized_codex_composer_text(parser.screen()), None);
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            None
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "wait through modal".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    assert!(
        app.ws()
            .panes
            .get(&codex_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "unknown modal layout must not receive PTY input"
    );
    app.shutdown();
}

#[test]
fn transcript_prompt_separated_from_unknown_output_is_not_live() {
    let mut parser = vt100::Parser::new(40, 120, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[4;1H\xE2\x80\xBA old request\x1b[7;1Hstreamed tool output\x1b[4;3H",
    );

    assert_eq!(codex_composer_has_draft_on_screen(parser.screen()), None);
    assert_eq!(normalized_codex_composer_text(parser.screen()), None);
    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
        None
    );
}

#[test]
fn ordinary_transcript_output_rows_are_not_structural_footers() {
    for output in [
        "reading src/app.rs",
        "reading src/low-level.rs",
        "reading crates/high-perf/mod.rs",
        "the throughput is high",
    ] {
        let mut bottom_parser = vt100::Parser::new(40, 120, 0);
        let screen = format!(
            "\x1b[?25h\x1b[2J\x1b[38;1H\u{203a} please refactor the parser\x1b[40;1H  {output}\x1b[38;3H"
        );
        bottom_parser.process(screen.as_bytes());
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(bottom_parser.screen()),
            None,
            "transcript output at the last screen row must be rejected: {output}"
        );
    }

    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[4;1H\xE2\x80\xBA please refactor the parser\x1b[6;1H  reading src/low-level.rs\x1b[4;3H",
    );
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(codex_composer_has_draft_on_screen(parser.screen()), None);
        assert_eq!(normalized_codex_composer_text(parser.screen()), None);
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            None
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "do not inject into transcript".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    assert!(
        app.ws()
            .panes
            .get(&codex_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "transcript output must not receive PTY input"
    );
    app.shutdown();
}

#[test]
fn persistently_unknown_screen_surfaces_notification_on_focus() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[Hfuture Codex layout",
    );
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "unknown screen".to_string(),
    )
    .expect("peer send");

    for _ in 0..8 {
        app.flush_pending_codex_peer_messages();
    }
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    if let Some(PendingCodexPeerDelivery::Draft { stalled_since, .. }) = app
        .pending_codex_peer_messages
        .get_mut(&codex_id)
        .and_then(|queue| queue.front_mut())
    {
        *stalled_since = Instant::now() - CODEX_PEER_DRAFT_STALL_TIMEOUT;
    }
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::AwaitFocus { retries_remaining, .. })
            if *retries_remaining == CODEX_PEER_NUDGE_MAX_RETRIES
    ));
    assert!(
        app.ws()
            .panes
            .get(&codex_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "unknown screens must not receive PTY input"
    );

    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus unknown Codex pane");
    assert!(app.visible_codex_peer_notification().is_some());
    app.shutdown();
}

#[test]
fn unknown_unactionable_screen_eventually_surfaces_on_focus() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Reticulating (12s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false)
        );
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(true)
        );
    }
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "recognized stall".to_string(),
    )
    .expect("peer send");
    if let Some(PendingCodexPeerDelivery::Draft { stalled_since, .. }) = app
        .pending_codex_peer_messages
        .get_mut(&codex_id)
        .and_then(|queue| queue.front_mut())
    {
        *stalled_since = Instant::now() - CODEX_PEER_DRAFT_STALL_TIMEOUT;
    }
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));
    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus stalled Codex pane");
    assert!(app.visible_codex_peer_notification().is_some());
    app.shutdown();
}

#[test]
fn transient_unknown_frames_preserve_native_queue_path() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(&mut app, codex_id, b"\x1b[?25h\x1b[2J");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "survive partial frames".to_string(),
    )
    .expect("peer send");

    for _ in 0..8 {
        app.flush_pending_codex_peer_messages();
    }
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));

    seed_codex_busy_placeholder(&mut app, codex_id);
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    app.shutdown();
}

#[test]
fn live_idle_prompt_with_distant_footer_accepts_nudge() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[1;1HReady\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[5;1Hwrapped composer row 1\x1b[6;1Hwrapped composer row 2\x1b[7;1Hwrapped composer row 3\x1b[8;1Hwrapped composer row 4\x1b[9;1Hwrapped composer row 5\x1b[10;1Hwrapped composer row 6\x1b[11;1Hwrapped composer row 7\x1b[12;1Hwrapped composer row 8\x1b[14;1Ho3 high \xC2\xB7 cwd\x1b[1;3H",
    );
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(true)
        );
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false)
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "idle prompt".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    assert!(
        !app.ws()
            .panes
            .get(&codex_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "validated live composer should receive the peer nudge"
    );
    app.shutdown();
}

#[test]
fn non_gpt_footer_is_live_with_cursor_parked_on_footer() {
    let mut parser = vt100::Parser::new(40, 120, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1Ho3 high \xC2\xB7 cwd\x1b[6;10H",
    );

    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
        Some(true)
    );
    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(false)
    );
}

#[test]
fn indented_empty_composer_accepts_nudge_without_waiting() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[4;5H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1Ho3 high \xC2\xB7 cwd\x1b[4;7H",
    );
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false)
        );
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(true)
        );
    }

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "indented composer".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn wrapped_legacy_enter_to_send_footer_is_recognized() {
    let mut parser = vt100::Parser::new(20, 12, 0);
    parser.process(b"\x1b[?25h\x1b[2J\x1b[1;1H\xE2\x80\xBA \x1b[3;5Henter to send\x1b[1;3H");

    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
        Some(true)
    );
}

#[test]
fn codex_prompt_allows_nudge_with_cursor_parked_on_footer() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1H  gpt-5.6-sol high \xC2\xB7 cwd\x1b[6;20H",
    );

    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
        Some(true)
    );
}

#[test]
fn flush_pending_codex_peer_messages_does_not_interrupt_existing_codex_draft() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");

    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        let mut parser = pane.parser.lock().unwrap();
        parser.process(
            b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA typed draft\n\n  gpt-5.4 high \xC2\xB7 cwd",
        );
    }
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "Codex pane with an existing draft should keep the nudge queued"
    );

    seed_codex_live_ready_placeholder(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "placeholder prompt should advance to submit stage once the pane is clean"
    );
    seed_expected_codex_peer_composer(&mut app, sibling_id, sender_id);
    make_codex_submit_ready(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "clean Codex prompt should eventually submit the queued nudge"
    );
    app.shutdown();
}

#[test]
fn focused_codex_without_draft_auto_submits_when_ready() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");
    seed_codex_live_ready_placeholder(&mut app, sibling_id);

    let outcome = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "hello focused codex".to_string(),
        )
        .expect("peer send");

    assert_eq!(outcome, ipc::PeerSendOutcome::Delivered);
    assert!(app.visible_codex_peer_notification().is_none());
    let queued = app
        .pending_codex_peer_messages
        .get(&sibling_id)
        .expect("submit should be delayed");
    assert!(matches!(
        queued.front(),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn focused_codex_without_draft_queues_when_not_ready() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");

    let outcome = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "hello focused codex".to_string(),
        )
        .expect("peer send");

    assert_eq!(outcome, ipc::PeerSendOutcome::Delivered);
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn focused_codex_new_draft_queue_does_not_inherit_existing_confirmation_outcome() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");

    seed_codex_draft(&mut app, sibling_id);
    let first = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "first notification".to_string(),
        )
        .expect("first peer send");
    assert_eq!(first, ipc::PeerSendOutcome::PendingUserConfirmation);

    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .parser = std::sync::Arc::new(std::sync::Mutex::new(vt100::Parser::new(40, 80, 10_000)));
    seed_codex_busy_placeholder(&mut app, sibling_id);
    {
        let pane = app.ws().panes.get(&sibling_id).expect("pane");
        let parser = pane
            .parser
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false),
            "the user-cleared busy composer must take the Draft queue path"
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();
    let second = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(sibling_id),
            "second automatic nudge".to_string(),
        )
        .expect("second peer send");

    assert_eq!(
        second,
        ipc::PeerSendOutcome::Delivered,
        "a newly queued Draft must not inherit an older notification's outcome"
    );
    assert_eq!(
        app.visible_codex_peer_notification()
            .expect("the first notification remains visible")
            .pending_count,
        1
    );
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));

    seed_codex_live_ready_placeholder(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    assert!(
        !app
            .ws()
            .panes
            .get(&sibling_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "the second nudge should enter automatic submission without accepting the first notification"
    );
    app.shutdown();
}

#[test]
fn unfocused_codex_with_draft_stays_silent_and_queued() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_codex_draft(&mut app, sibling_id);

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn unfocused_busy_codex_without_draft_queues_nudge_natively() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_codex_busy_placeholder(&mut app, sibling_id);
    {
        let pane = app.ws().panes.get(&sibling_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false)
        );
        assert!(!parser.screen().hide_cursor());
        let tail = screen_tail_lines(parser.screen())
            .join("\n")
            .to_ascii_lowercase();
        assert!(tail.contains("esc to interrupt"), "{tail:?}");
        assert!(!tail.contains("tab to queue message"), "{tail:?}");
    }

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "correction while working".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(app.visible_codex_peer_notification().is_none());
    let pending = app
        .pending_codex_peer_messages
        .get(&sibling_id)
        .and_then(|q| q.front());
    assert!(
        matches!(pending, Some(PendingCodexPeerDelivery::QueueAt { .. })),
        "busy Codex nudge should advance to native queue stage: {pending:?}"
    );

    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    seed_codex_busy_composer(&mut app, sibling_id, &expected);
    {
        let pane = app.ws().panes.get(&sibling_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        let observed = normalized_codex_composer_text(parser.screen()).unwrap_or_else(|| {
            panic!(
                "composer not found: {:?}",
                screen_tail_lines(parser.screen())
            )
        });
        assert_eq!(
            observed,
            expected
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();
    make_codex_native_queue_ready(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "Tab should commit the nudge to Codex's native queue"
    );
    assert_eq!(
        app.ws().panes.get(&sibling_id).expect("pane").test_input(),
        b"\t"
    );
    app.shutdown();
}

#[test]
fn busy_codex_native_queue_path_is_independent_of_pane_focus() {
    for pane_is_focused in [false, true] {
        let mut app = App::new(40, 160).expect("App::new");
        let sender_id = app.ws().focused_pane_id;
        let codex_id = app
            .handle_split(
                &ipc::PaneRef::Focused,
                ipc::Direction::Vertical,
                None,
                None,
                None,
                None,
            )
            .expect("split succeeds");
        app.peer_client_kinds
            .insert(codex_id, PeerClientKind::Codex);
        app.peer_delivery_ready.insert(codex_id);
        let focused_id = if pane_is_focused { codex_id } else { sender_id };
        app.handle_focus(&ipc::PaneRef::Id(focused_id))
            .expect("set explicit pane focus");
        seed_codex_busy_placeholder(&mut app, codex_id);

        app.handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(codex_id),
            format!("focus={pane_is_focused}"),
        )
        .expect("peer send");
        app.flush_pending_codex_peer_messages();

        let pending = app
            .pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front());
        assert!(
            matches!(pending, Some(PendingCodexPeerDelivery::QueueAt { .. })),
            "the same busy screen must use native queue regardless of pane focus: {pending:?}"
        );
        assert!(app.visible_codex_peer_notification().is_none());

        let expected = format_codex_peer_message(&PendingCodexPeerMessage {
            from_pane: sender_id,
            from_name: None,
            from_kind: None,
        });
        seed_codex_busy_composer(&mut app, codex_id, &expected);
        app.ws_mut()
            .panes
            .get_mut(&codex_id)
            .expect("pane")
            .clear_test_input();
        make_codex_native_queue_ready(&mut app, codex_id);
        app.flush_pending_codex_peer_messages();

        assert_eq!(
            app.ws().panes.get(&codex_id).expect("pane").test_input(),
            b"\t",
            "native queue commit must use Tab when pane_is_focused={pane_is_focused}"
        );
        assert!(!app.pending_codex_peer_messages.contains_key(&codex_id));
        app.shutdown();
    }
}

#[test]
fn focused_native_queue_never_falls_back_to_enter_when_turn_finishes() {
    for injected_while_focused in [false, true] {
        let mut app = App::new(40, 160).expect("App::new");
        let sender_id = app.ws().focused_pane_id;
        let codex_id = app
            .handle_split(
                &ipc::PaneRef::Focused,
                ipc::Direction::Vertical,
                None,
                None,
                None,
                None,
            )
            .expect("split succeeds");
        app.peer_client_kinds
            .insert(codex_id, PeerClientKind::Codex);
        app.peer_delivery_ready.insert(codex_id);
        let initial_focus = if injected_while_focused {
            codex_id
        } else {
            sender_id
        };
        app.handle_focus(&ipc::PaneRef::Id(initial_focus))
            .expect("set initial pane focus");
        seed_codex_busy_placeholder(&mut app, codex_id);

        app.handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(codex_id),
            format!("turn finishes before Tab; injected_focused={injected_while_focused}"),
        )
        .expect("peer send");
        app.flush_pending_codex_peer_messages();

        let expected = format_codex_peer_message(&PendingCodexPeerMessage {
            from_pane: sender_id,
            from_name: None,
            from_kind: None,
        });
        app.handle_focus(&ipc::PaneRef::Id(codex_id))
            .expect("focus Codex before commit");
        seed_codex_idle_composer(&mut app, codex_id, &expected);
        app.ws_mut()
            .panes
            .get_mut(&codex_id)
            .expect("pane")
            .clear_test_input();
        make_codex_native_queue_ready(&mut app, codex_id);
        app.flush_pending_codex_peer_messages();

        assert_eq!(
            app.ws().panes.get(&codex_id).expect("pane").test_input(),
            b"\x15",
            "renga must clear its matching composer instead of pressing Enter; injected_focused={injected_while_focused}"
        );
        assert!(app.visible_codex_peer_notification().is_some());
        assert!(!app.pending_codex_peer_messages.contains_key(&codex_id));
        app.shutdown();
    }
}

#[test]
fn unfocused_queue_fallback_does_not_replace_another_panes_notification() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let pane_a = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split pane A");
    let pane_b = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Horizontal,
            None,
            None,
            None,
            None,
        )
        .expect("split pane B");
    for pane_id in [pane_a, pane_b] {
        app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
        app.peer_delivery_ready.insert(pane_id);
    }

    app.handle_focus(&ipc::PaneRef::Id(pane_b))
        .expect("focus pane B");
    seed_codex_busy_placeholder(&mut app, pane_b);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(pane_b),
        "queue for pane B".to_string(),
    )
    .expect("send to pane B");
    app.flush_pending_codex_peer_messages();
    let expected_b = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    seed_codex_idle_composer(&mut app, pane_b, &expected_b);

    app.handle_focus(&ipc::PaneRef::Id(pane_a))
        .expect("focus pane A");
    seed_codex_draft(&mut app, pane_a);
    let outcome = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(pane_a),
            "retain pane A notification".to_string(),
        )
        .expect("send to pane A");
    assert_eq!(outcome, ipc::PeerSendOutcome::PendingUserConfirmation);
    assert_eq!(
        app.visible_codex_peer_notification()
            .expect("pane A notification")
            .target_pane,
        pane_a
    );

    app.ws_mut()
        .panes
        .get_mut(&pane_b)
        .expect("pane B")
        .clear_test_input();
    make_codex_native_queue_ready(&mut app, pane_b);
    app.flush_pending_codex_peer_messages();

    assert_eq!(
        app.visible_codex_peer_notification()
            .expect("pane A notification must remain")
            .target_pane,
        pane_a,
        "an unfocused pane must not replace another pane's visible notification"
    );
    assert_eq!(
        app.ws().panes.get(&pane_b).expect("pane B").test_input(),
        b"\r",
        "the now-unfocused pane may use the ordinary idle Enter fallback"
    );
    app.shutdown();
}

#[test]
fn focused_expired_queue_with_changed_composer_does_not_clear_user_input() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "do not clear changed composer".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus Codex pane");
    seed_codex_busy_composer(&mut app, codex_id, "user changed this composer");
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    expire_codex_native_queue(&mut app, codex_id);
    app.flush_pending_codex_peer_messages();

    assert!(
        app.ws()
            .panes
            .get(&codex_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "a mismatched composer may belong to the user and must not receive Ctrl+U"
    );
    assert!(app.visible_codex_peer_notification().is_some());
    app.shutdown();
}

#[test]
fn busy_codex_uses_structured_status_row() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (48s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA \x1b[2mImprove documentation in @filename\x1b[22m\x1b[6;1H  gpt-5.6-sol high \xC2\xB7 cwd\x1b[1;9H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "status cursor".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    app.shutdown();
}

#[test]
fn thinking_status_uses_native_queue() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Thinking (12s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "thinking status".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    app.shutdown();
}

#[test]
fn waiting_for_background_terminal_field_status_uses_native_queue() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    let status = "• Waiting for background terminal (8m 55s • esc to interrupt) · 1 background terminal running · /ps to view · /st…";
    let command = "└ python .repro/seal_gameocr_c5rl_causal_trace.py";
    let empty_screen = format!(
        "\x1b[?25h\x1b[2J\x1b[H{status}\x1b[2;1H{command}\x1b[4;1H\u{203a} \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1H  gpt-5.6-sol medium \u{b7} ~\\Develop\\gameocr\x1b[4;3H"
    );
    seed_pane_screen(&mut app, codex_id, empty_screen.as_bytes());

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "field background wait".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    assert!(
        !app.ws()
            .panes
            .get(&codex_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "the measured busy status must allow the draft write"
    );

    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    let chars = expected.chars().collect::<Vec<_>>();
    let mut queued_screen = format!("\x1b[?25h\x1b[2J\x1b[H{status}\x1b[2;1H{command}");
    for (index, chunk) in chars.chunks(60).enumerate() {
        let row = 4 + index;
        let text = chunk.iter().collect::<String>();
        let prefix = if index == 0 { "\u{203a} " } else { "  " };
        queued_screen.push_str(&format!("\x1b[{row};1H{prefix}{text}"));
    }
    let footer_row = 5 + chars.chunks(60).len();
    let cursor_row = 3 + chars.chunks(60).len();
    let cursor_col = chars.chunks(60).last().map_or(3, |chunk| chunk.len() + 3);
    queued_screen.push_str(&format!(
        "\x1b[{footer_row};1H  tab to queue message                40% context left\x1b[{cursor_row};{cursor_col}H"
    ));
    seed_pane_screen(&mut app, codex_id, queued_screen.as_bytes());
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    make_codex_native_queue_ready(&mut app, codex_id);

    app.flush_pending_codex_peer_messages();

    assert_eq!(
        app.ws().panes.get(&codex_id).expect("pane").test_input(),
        b"\t",
        "Codex's queue hint must gate the Tab commit"
    );
    app.shutdown();
}

#[test]
fn truncated_known_busy_status_never_falls_through_to_enter() {
    for status in [
        "◦ Waiting for background terminal (1m 24s • esc to inte…",
        "◦ Thinking (48s • esc to inte…",
    ] {
        let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
        let empty_screen = format!(
            "\x1b[?25h\x1b[2J\x1b[H{status}\x1b[4;1H\u{203a} \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1H  gpt-5.6-sol medium \u{b7} cwd\x1b[4;3H"
        );
        seed_pane_screen(&mut app, codex_id, empty_screen.as_bytes());
        app.handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(codex_id),
            "truncated busy status".to_string(),
        )
        .expect("peer send");
        app.flush_pending_codex_peer_messages();
        assert!(matches!(
            app.pending_codex_peer_messages
                .get(&codex_id)
                .and_then(|q| q.front()),
            Some(PendingCodexPeerDelivery::QueueAt { .. })
        ));

        let expected = format_codex_peer_message(&PendingCodexPeerMessage {
            from_pane: sender_id,
            from_name: None,
            from_kind: None,
        });
        let draft_screen = format!(
            "\x1b[?25h\x1b[2J\x1b[H{status}\x1b[4;1H\u{203a} {expected}\x1b[8;1H  gpt-5.6-sol medium \u{b7} cwd\x1b[4;{}H",
            expected.chars().count() + 3
        );
        seed_pane_screen(&mut app, codex_id, draft_screen.as_bytes());
        app.ws_mut()
            .panes
            .get_mut(&codex_id)
            .expect("pane")
            .clear_test_input();
        make_codex_native_queue_ready(&mut app, codex_id);

        app.flush_pending_codex_peer_messages();

        assert!(
            !app.ws()
                .panes
                .get(&codex_id)
                .expect("pane")
                .test_input()
                .contains(&b'\r'),
            "a truncated {status} status must not select Enter before the Tab hint appears"
        );
        assert!(matches!(
            app.pending_codex_peer_messages
                .get(&codex_id)
                .and_then(|q| q.front()),
            Some(PendingCodexPeerDelivery::QueueAt { .. })
        ));
        app.shutdown();
    }
}

#[test]
fn transcript_thinking_without_numeric_elapsed_is_not_busy() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HThinking (see below) is transcript text, not a live status.\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "numeric elapsed required".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn nearby_transcript_interrupt_phrase_uses_neither_automatic_path() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HDuring a turn Codex prints esc to interrupt in its status row.\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "nearby transcript".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn wrapped_busy_status_blocks_both_queue_and_idle_paths() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (1m 03s \xE2\x80\xA2 es\x1b[2;1Hc to interrupt)\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "wrapped status".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    app.shutdown();
}

#[test]
fn unfamiliar_interrupt_status_blocks_both_queue_and_idle_paths() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Reticulating (12s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "unfamiliar status".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    app.shutdown();
}

#[test]
fn split_transcript_words_use_neither_automatic_path() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HTo stop a run you press Esc to\x1b[2;1Hinterrupt it, then retry.\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "row separator".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn distant_transcript_interrupt_hint_does_not_mark_prompt_busy() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (12s \xE2\x80\xA2 esc to interrupt)\x1b[8;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[10;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[8;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "positioned status".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn busy_codex_nudge_submits_if_turn_finishes_before_tab() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_codex_busy_placeholder(&mut app, sibling_id);

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "correction near turn completion".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));

    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    // Codex v0.147.0 has no idle action hint. Turn completion is observable
    // only through the disappearance of the busy status above the composer.
    let idle_screen = format!(
        "\x1b[?25h\x1b[2J\x1b[H\u{203a} {expected}\x1b[5;1Hgpt-5.6-sol medium \u{b7} cwd\x1b[3;20H"
    );
    seed_pane_screen(&mut app, sibling_id, idle_screen.as_bytes());
    {
        let pane = app.ws().panes.get(&sibling_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            normalized_codex_composer_text(parser.screen()),
            Some(expected.chars().filter(|ch| !ch.is_whitespace()).collect())
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();
    make_codex_native_queue_ready(&mut app, sibling_id);
    app.flush_pending_codex_peer_messages();

    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "Enter should commit the injected nudge after Codex becomes idle"
    );
    assert_eq!(
        app.ws().panes.get(&sibling_id).expect("pane").test_input(),
        b"\r"
    );
    app.shutdown();
}

#[test]
fn unfocused_idle_codex_without_draft_advances_to_submit_stage() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_codex_live_ready_placeholder(&mut app, sibling_id);

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "idle nudge".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn idle_transcript_queue_hint_does_not_select_native_queue() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HI explained that Tab to queue message is how Codex queues\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[6;1H  gpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "idle transcript false positive".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn native_queue_commit_does_not_send_changed_user_draft() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "do not send user draft".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA what happens if I\x1b[3;1H  gpt-5.6-sol medium \xC2\xB7 cwd\x1b[1;20H",
    );
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    make_codex_native_queue_ready(&mut app, codex_id);

    app.flush_pending_codex_peer_messages();

    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    app.shutdown();
}

#[test]
fn submit_commit_does_not_send_changed_composer() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");
    seed_codex_live_ready_placeholder(&mut app, codex_id);

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "guard delayed submit".to_string(),
    )
    .expect("peer send");
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    let changed = format!("{expected}ABC");
    seed_codex_idle_composer(&mut app, codex_id, &changed);
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            normalized_codex_composer_text(parser.screen()),
            Some(changed.chars().filter(|ch| !ch.is_whitespace()).collect()),
            "fixture must expose the changed composer before commit"
        );
    }
    make_codex_submit_ready(&mut app, codex_id);

    app.flush_pending_codex_peer_messages();

    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn queue_hint_outside_footer_range_never_falls_through_to_enter() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "narrow pane".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    let chars = expected.chars().collect::<Vec<_>>();
    let mut screen =
        String::from("\x1b[?25h\x1b[2J\x1b[H\u{25e6} Working (1m 03s \u{2022} esc to interrupt)");
    for (index, chunk) in chars.chunks(20).enumerate() {
        let row = 4 + index;
        let text = chunk.iter().collect::<String>();
        let prefix = if index == 0 { "\u{203a} " } else { "  " };
        screen.push_str(&format!("\x1b[{row};1H{prefix}{text}"));
    }
    let blank_row = 4 + chars.chunks(20).len();
    let footer_row = blank_row + 1;
    screen.push_str(&format!(
        "\x1b[{footer_row};1H  tab to queue message  51% context left\x1b[4;3H"
    ));
    seed_pane_screen(&mut app, codex_id, screen.as_bytes());
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            normalized_codex_composer_text(parser.screen()),
            Some(expected.chars().filter(|ch| !ch.is_whitespace()).collect())
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    make_codex_native_queue_ready(&mut app, codex_id);

    app.flush_pending_codex_peer_messages();

    // The composer matches exactly, so remaining pending here specifically
    // verifies that a footer outside the measured range cannot select a key.
    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    app.shutdown();
}

#[test]
fn native_queue_commit_times_out_to_pending_on_unknown_screen() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "wait through approval".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HAllow command `cargo publish`?\x1b[3;1HYes / No\x1b[3;1H",
    );
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    expire_codex_native_queue(&mut app, codex_id);

    app.flush_pending_codex_peer_messages();

    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn native_queue_commit_stops_after_one_retry_and_waits_for_focus() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "bounded retry".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    let message = PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    };
    let expected = format_codex_peer_message(&message);
    seed_codex_busy_composer(&mut app, codex_id, &expected);
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    expire_codex_native_queue(&mut app, codex_id);
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft {
            retries_remaining: 0,
            ..
        })
    ));

    seed_codex_busy_placeholder(&mut app, codex_id);
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt {
            retries_remaining: 0,
            ..
        })
    ));

    seed_codex_busy_composer(&mut app, codex_id, &expected);
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    expire_codex_native_queue(&mut app, codex_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.ws().panes.get(&codex_id).expect("pane").test_input(),
        b"\x15"
    );
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));

    seed_codex_busy_placeholder(&mut app, codex_id);
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    for _ in 0..3 {
        app.flush_pending_codex_peer_messages();
    }
    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));

    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");
    assert!(app.visible_codex_peer_notification().is_some());
    app.shutdown();
}

#[test]
fn long_wrapped_composer_is_detected_and_surfaces_on_focus() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "long composer".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    seed_codex_long_busy_composer(&mut app, codex_id, &expected);
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            normalized_codex_composer_text(parser.screen()),
            Some(expected.chars().filter(|ch| !ch.is_whitespace()).collect())
        );
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(true)
        );
    }
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();
    expire_codex_native_queue(&mut app, codex_id);
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft {
            retries_remaining: 0,
            ..
        })
    ));

    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");
    assert!(app.visible_codex_peer_notification().is_some());
    app.shutdown();
}

#[test]
fn await_focus_nudge_resumes_when_unfocused_pane_becomes_idle() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    app.pending_codex_peer_messages.insert(
        codex_id,
        VecDeque::from([PendingCodexPeerDelivery::AwaitFocus {
            message: PendingCodexPeerMessage {
                from_pane: sender_id,
                from_name: None,
                from_kind: None,
            },
            retries_remaining: 0,
            delivery_sequence: None,
        }]),
    );
    seed_codex_live_ready_placeholder(&mut app, codex_id);
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft {
            retries_remaining: 0,
            ..
        })
    ));
    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());

    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    assert!(!app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    app.shutdown();
}

#[test]
fn await_focus_resume_preserves_retry_budget() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    app.pending_codex_peer_messages.insert(
        codex_id,
        VecDeque::from([PendingCodexPeerDelivery::AwaitFocus {
            message: PendingCodexPeerMessage {
                from_pane: sender_id,
                from_name: None,
                from_kind: None,
            },
            retries_remaining: CODEX_PEER_NUDGE_MAX_RETRIES,
            delivery_sequence: None,
        }]),
    );
    seed_codex_live_ready_placeholder(&mut app, codex_id);

    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft {
            retries_remaining: CODEX_PEER_NUDGE_MAX_RETRIES,
            ..
        })
    ));
    app.shutdown();
}

#[test]
fn await_focus_nudge_does_not_resume_over_footer_parked_draft() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    app.pending_codex_peer_messages.insert(
        codex_id,
        VecDeque::from([PendingCodexPeerDelivery::AwaitFocus {
            message: PendingCodexPeerMessage {
                from_pane: sender_id,
                from_name: None,
                from_kind: None,
            },
            retries_remaining: 0,
            delivery_sequence: None,
        }]),
    );
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HPrevious output\x1b[4;1H\xE2\x80\xBA keep my draft\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[6;20H",
    );
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));
    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    app.shutdown();
}

#[test]
fn exhausted_notification_stays_parked_after_focus_leaves() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    app.pending_codex_peer_messages.insert(
        codex_id,
        VecDeque::from([PendingCodexPeerDelivery::AwaitFocus {
            message: PendingCodexPeerMessage {
                from_pane: sender_id,
                from_name: None,
                from_kind: None,
            },
            retries_remaining: 0,
            delivery_sequence: None,
        }]),
    );
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");
    assert_eq!(
        app.visible_codex_peer_notification()
            .expect("visible notification")
            .retries_remaining,
        None
    );

    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("leave codex");
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));
    assert!(app
        .ws()
        .panes
        .get(&codex_id)
        .expect("pane")
        .test_input()
        .is_empty());
    app.shutdown();
}

#[test]
fn coalesced_notification_keeps_largest_retry_budget() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");
    app.codex_peer_notification = Some(CodexPeerNotificationState {
        target_pane: codex_id,
        message: PendingCodexPeerMessage {
            from_pane: sender_id,
            from_name: None,
            from_kind: None,
        },
        pending_count: 1,
        retries_remaining: Some(CODEX_PEER_NUDGE_MAX_RETRIES),
    });
    app.pending_codex_peer_messages.insert(
        codex_id,
        VecDeque::from([PendingCodexPeerDelivery::AwaitFocus {
            message: PendingCodexPeerMessage {
                from_pane: sender_id,
                from_name: Some("later".to_string()),
                from_kind: None,
            },
            retries_remaining: 0,
            delivery_sequence: None,
        }]),
    );

    app.flush_pending_codex_peer_messages();

    let notification = app
        .visible_codex_peer_notification()
        .expect("coalesced notification");
    assert_eq!(notification.pending_count, 2);
    assert_eq!(
        notification.retries_remaining,
        Some(CODEX_PEER_NUDGE_MAX_RETRIES),
        "an eligible coalesced message keeps a retry budget for the notification"
    );
    app.shutdown();
}

#[test]
fn focusing_injected_nudge_keeps_native_queue_commit_pending() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "focus during delay".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    seed_codex_busy_composer(&mut app, codex_id, &expected);
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");

    assert!(
        app.ws()
            .panes
            .get(&codex_id)
            .expect("pane")
            .test_input()
            .is_empty(),
        "focus alone must not commit or clear the injected nudge"
    );
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));

    make_codex_native_queue_ready(&mut app, codex_id);
    app.flush_pending_codex_peer_messages();

    assert_eq!(
        app.ws().panes.get(&codex_id).expect("pane").test_input(),
        b"\t",
        "a focused busy pane may commit only through Codex's native Tab queue"
    );
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(!app.pending_codex_peer_messages.contains_key(&codex_id));
    app.shutdown();
}

#[test]
fn second_message_does_not_discard_in_flight_native_queue_commit() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(codex_id), "first".to_string())
        .expect("first send");
    app.flush_pending_codex_peer_messages();
    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    seed_codex_busy_composer(&mut app, codex_id, &expected);
    app.ws_mut().focused_pane_id = codex_id;

    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(codex_id), "second".to_string())
        .expect("second send");

    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    assert!(app.codex_peer_notification.is_none());
    app.shutdown();
}

#[test]
fn unfocused_busy_codex_with_draft_does_not_inject_nudge() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(sibling_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_pane_screen(
        &mut app,
        sibling_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (1m 03s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA keep my draft\x1b[6;1H  gpt-5.6-sol high \xC2\xB7 cwd\x1b[4;16H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "do not overwrite draft".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.shutdown();
}

#[test]
fn refocusing_unfocused_codex_with_existing_draft_shows_pending_overlay() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let pane_b = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds.insert(pane_a, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(pane_a);
    seed_codex_draft(&mut app, pane_a);
    assert_eq!(app.ws().focused_pane_id, pane_b);

    app.handle_peer_send(pane_b, &ipc::PaneRef::Id(pane_a), "draft ping".to_string())
        .expect("peer send");
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&pane_a)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));

    app.handle_focus(&ipc::PaneRef::Id(pane_a))
        .expect("refocus draft pane");

    let notification = app
        .visible_codex_peer_notification()
        .expect("pending nudge should become a focused overlay");
    assert_eq!(notification.target_pane, pane_a);
    assert_eq!(notification.pending_count, 1);
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_a));
    app.shutdown();
}

#[test]
fn focused_codex_pending_queue_auto_submits_after_draft_clears_to_unknown_placeholder() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");
    seed_codex_draft(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "draft ping".to_string(),
    )
    .expect("peer send");
    assert!(app.visible_codex_peer_notification().is_some());

    app.requeue_codex_peer_notification();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA Write tests for @filename\r\n\r\n  gpt-5.6-sol medium \xC2\xB7 cwd\x1b[1;3H",
    );
    app.flush_pending_codex_peer_messages();

    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}
#[test]
fn focused_codex_pending_overlay_requeues_when_typing_then_auto_submits_after_draft_clears() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus codex");
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA x\x1b[1;5H",
    );
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "draft ping".to_string(),
    )
    .expect("peer send");
    assert!(app.visible_codex_peer_notification().is_some());

    let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
    let consumed = app.handle_key_event(backspace).expect("route backspace");
    assert!(!consumed);
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));

    seed_codex_live_ready_placeholder(&mut app, codex_id);
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}
#[test]
fn focus_transition_routes_pending_codex_by_draft_state() {
    let mut app = App::new(40, 160).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let draft_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(draft_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(draft_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_codex_draft(&mut app, draft_id);
    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(draft_id), "draft".to_string())
        .expect("peer send");

    app.handle_focus(&ipc::PaneRef::Id(draft_id))
        .expect("focus draft pane");
    assert!(app.visible_codex_peer_notification().is_some());

    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    let ready_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(ready_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(ready_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
    seed_codex_live_ready_placeholder(&mut app, ready_id);
    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(ready_id), "ready".to_string())
        .expect("peer send");

    app.handle_focus(&ipc::PaneRef::Id(ready_id))
        .expect("focus ready pane");
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&ready_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn dim_codex_placeholder_is_not_a_draft() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA \x1b[2mAsk Codex anything...\x1b[22m\x1b[1;3H",
    );

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(false)
    );
}

#[test]
fn unknown_codex_placeholder_at_prompt_is_not_a_draft_without_dim() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA Write tests for @filename\x1b[1;3H");

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(false)
    );
}
#[test]
fn colored_codex_placeholder_is_not_a_draft_when_cursor_is_at_prompt() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA \x1b[38;5;240mAsk Codex anything...\x1b[39m\x1b[1;3H",
    );

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(false)
    );
}

#[test]
fn codex_composer_draft_uses_cursor_position_on_prompt_row() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA typed draft\x1b[1;15H");

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(true)
    );
}

#[test]
fn codex_composer_draft_uses_cursor_position_below_prompt_row() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA \nsecond line\x1b[2;12H");

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(true)
    );
}

#[test]
fn codex_empty_composer_ignores_cursor_parked_on_footer() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (8s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA \x1b[2mImprove documentation in @filename\x1b[22m\x1b[6;1H  gpt-5.6-sol high \xC2\xB7 cwd\x1b[6;20H",
    );

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(false)
    );
}

#[test]
fn codex_real_draft_remains_protected_with_cursor_parked_on_footer() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Working (8s \xE2\x80\xA2 esc to interrupt)\x1b[4;1H\xE2\x80\xBA keep my draft\x1b[6;1H  gpt-5.6-sol high \xC2\xB7 cwd\x1b[6;20H",
    );

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(true)
    );
}
#[test]
fn handle_peer_list_excludes_caller_and_lists_siblings() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("sibling".into()),
            Some("worker".into()),
            None,
        )
        .expect("split succeeds");
    let peers = app.handle_peer_list(sender_id).expect("peer list");
    assert_eq!(peers.len(), 1, "expected one sibling, got {peers:?}");
    assert_eq!(peers[0].id, sibling_id);
    assert_eq!(peers[0].name.as_deref(), Some("sibling"));
    assert_eq!(peers[0].role.as_deref(), Some("worker"));
    // Caller must be excluded.
    assert!(
        peers.iter().all(|p| p.id != sender_id),
        "peer list must not include the caller"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_dedupes_identical_payload_within_window() {
    // renga#221 acceptance criterion #2: re-sending the exact same
    // payload from the same peer within the dedupe window must not
    // produce two PeerInbox events. Otherwise a chatty dispatcher /
    // worker can paper the receiver's transcript with phantom
    // Human: turns.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_register_client(sibling_id, PeerClientKind::Claude)
        .expect("peer registration");
    app.handle_peer_set_ready(sibling_id, PeerClientKind::Claude, true)
        .expect("peer readiness");
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "ack".to_string())
        .expect("first send");
    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "ack".to_string())
        .expect("second send (duplicate)");

    let mut peer_inboxes = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox { body, .. } = ev {
            assert_eq!(body, "ack");
            peer_inboxes += 1;
        }
    }
    assert_eq!(
        peer_inboxes, 1,
        "duplicate identical payload should collapse to a single PeerInbox"
    );
    app.shutdown();
}

#[test]
fn duplicate_for_unready_peer_still_reports_queued() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");

    let first = app
        .handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "same".to_string())
        .expect("first send");
    let duplicate = app
        .handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "same".to_string())
        .expect("duplicate send");

    assert_eq!(first, ipc::PeerSendOutcome::Queued);
    assert_eq!(duplicate, ipc::PeerSendOutcome::Queued);
    assert_eq!(app.pending_peer_inbox[&sibling_id].len(), 1);
    app.shutdown();
}

#[test]
fn spawned_codex_command_without_peer_registration_reports_queued() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let target_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            Some("codex".to_string()),
            None,
            None,
            None,
        )
        .expect("split succeeds");

    let peers = app.handle_peer_list(sender_id).expect("peer list succeeds");
    let target = peers
        .iter()
        .find(|peer| peer.id == target_id)
        .expect("spawned pane is listed");
    assert_eq!(target.kind, None);
    assert_eq!(target.receive_mode, None);
    assert!(!app.peer_delivery_ready.contains(&target_id));

    let outcome = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(target_id),
            "test message".to_string(),
        )
        .expect("send succeeds");

    assert_eq!(outcome, ipc::PeerSendOutcome::Queued);
    assert_eq!(app.pending_peer_inbox[&target_id].len(), 1);
    app.shutdown();
}

#[test]
fn subscriber_gone_revokes_pull_and_push_readiness_without_erasing_kind() {
    for kind in [PeerClientKind::Claude, PeerClientKind::Codex] {
        let mut app = App::new(40, 80).expect("App::new");
        let pane_id = app.ws().focused_pane_id;
        app.handle_peer_set_ready(pane_id, kind, true)
            .expect("peer readiness");
        assert!(app.peer_delivery_ready.contains(&pane_id));

        app.handle_peer_subscriber_gone(pane_id);

        assert!(!app.peer_delivery_ready.contains(&pane_id));
        assert_eq!(app.peer_client_kinds.get(&pane_id), Some(&kind));
        app.shutdown();
    }
}

#[test]
fn delivered_duplicate_replays_delivered_after_disconnect() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_set_ready(sibling_id, PeerClientKind::Claude, true)
        .expect("ready target");

    let first = app
        .handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "same".to_string())
        .expect("first send");
    app.handle_peer_set_ready(sibling_id, PeerClientKind::Claude, false)
        .expect("disconnect target");
    let duplicate = app
        .handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "same".to_string())
        .expect("duplicate send");

    assert_eq!(first, ipc::PeerSendOutcome::Delivered);
    assert_eq!(duplicate, ipc::PeerSendOutcome::Delivered);
    assert!(!app.pending_peer_inbox.contains_key(&sibling_id));
    app.shutdown();
}

#[test]
fn handle_peer_send_distinct_bodies_are_not_deduped() {
    // Sanity check: dedupe is keyed on body, so two genuinely
    // distinct messages must both go through. Without this, the
    // "every reply gets the same prefix" pattern would silently
    // swallow follow-ups.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_register_client(sibling_id, PeerClientKind::Claude)
        .expect("peer registration");
    app.handle_peer_set_ready(sibling_id, PeerClientKind::Claude, true)
        .expect("peer readiness");
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "first".to_string(),
    )
    .expect("first send");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "second".to_string(),
    )
    .expect("second send");

    let mut bodies = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox { body, .. } = ev {
            bodies.push(body);
        }
    }
    assert_eq!(bodies, vec!["first", "second"]);
    app.shutdown();
}

#[test]
fn handle_peer_send_dedupe_does_not_collapse_distinct_senders() {
    // Dedupe key is (target, from, body). Two different peers
    // sending the same text must both deliver, since they really
    // are independent messages in the human sense.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_a = app.ws().focused_pane_id;
    let sender_b = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds (sender_b)");
    let target = app
        .handle_split(
            &ipc::PaneRef::Id(sender_a),
            ipc::Direction::Horizontal,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds (target)");
    app.handle_peer_register_client(target, PeerClientKind::Claude)
        .expect("peer registration");
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .expect("peer readiness");
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(sender_a, &ipc::PaneRef::Id(target), "ping".to_string())
        .expect("a -> target");
    app.handle_peer_send(sender_b, &ipc::PaneRef::Id(target), "ping".to_string())
        .expect("b -> target");

    let mut count = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox { .. } = ev {
            count += 1;
        }
    }
    assert_eq!(
        count, 2,
        "same body from distinct senders must not collapse into one delivery"
    );
    app.shutdown();
}

fn app_with_ready_peer(
    kind: PeerClientKind,
) -> (App, usize, usize, std::sync::mpsc::Receiver<ipc::Event>) {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender = app.ws().focused_pane_id;
    let target = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.handle_peer_set_ready(target, kind, true)
        .expect("ready target");
    while rx.try_recv().is_ok() {}
    (app, sender, target, rx)
}

fn peer_delivery_id(rx: &std::sync::mpsc::Receiver<ipc::Event>) -> u64 {
    loop {
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("PeerInbox event")
        {
            ipc::Event::PeerInbox {
                delivery_id: Some(id),
                ..
            } => return id,
            _ => continue,
        }
    }
}

#[test]
fn production_send_waits_for_mcp_receipt_before_codex_nudge_and_reply() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender))
        .expect("focus sender");
    let (reply_tx, reply_rx) = oneshot::channel();

    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "race".into(), reply_tx);
    let delivery_id = peer_delivery_id(&rx);
    app.flush_pending_codex_peer_messages();
    assert!(!app.pending_codex_peer_messages.contains_key(&target));
    assert!(reply_rx.recv_timeout(Duration::from_millis(10)).is_err());

    app.handle_peer_inbox_ack(target, delivery_id)
        .expect("receipt");
    assert!(app.pending_codex_peer_messages.contains_key(&target));
    assert_eq!(
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("send reply")
            .expect("delivered"),
        ipc::PeerSendOutcome::Delivered
    );
    let nudge_count = app.pending_codex_peer_messages[&target].len();
    app.handle_peer_inbox_ack(target, delivery_id)
        .expect("duplicate receipt is stale");
    assert_eq!(app.pending_codex_peer_messages[&target].len(), nudge_count);
    app.shutdown();
}

#[test]
fn identical_inflight_sends_join_one_delivery_and_share_outcome() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Claude);
    let (reply_a_tx, reply_a_rx) = oneshot::channel();
    let (reply_b_tx, reply_b_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "same".into(), reply_a_tx);
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "same".into(), reply_b_tx);
    let delivery_id = peer_delivery_id(&rx);
    assert_eq!(app.pending_peer_deliveries.len(), 1);
    assert_eq!(app.pending_peer_deliveries[&delivery_id].replies.len(), 2);
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })));

    app.handle_peer_inbox_ack(target, delivery_id)
        .expect("receipt");
    for reply in [reply_a_rx, reply_b_rx] {
        assert_eq!(
            reply.recv().unwrap().unwrap(),
            ipc::PeerSendOutcome::Delivered
        );
    }
    app.shutdown();
}

#[test]
fn wrong_pane_cannot_confirm_delivery_and_stale_receipt_is_ignored() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Claude);
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "guard".into(), reply_tx);
    let delivery_id = peer_delivery_id(&rx);

    let error = app
        .handle_peer_inbox_ack(sender, delivery_id)
        .expect_err("wrong pane must fail");
    assert_eq!(error.code, Some(ipc::err_code::PROTOCOL));
    assert!(app.pending_peer_deliveries.contains_key(&delivery_id));
    app.handle_peer_inbox_ack(target, delivery_id)
        .expect("right pane");
    assert_eq!(
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap(),
        ipc::PeerSendOutcome::Delivered
    );
    app.handle_peer_inbox_ack(target, delivery_id + 999)
        .expect("unknown receipt ignored");
    app.shutdown();
}

#[test]
fn retry_reuses_delivery_id_and_timeout_allows_immediate_resend() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Claude);
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "retry".into(), reply_tx);
    let first_id = peer_delivery_id(&rx);
    app.pending_peer_deliveries
        .get_mut(&first_id)
        .unwrap()
        .next_retry_at = Instant::now();
    app.flush_pending_peer_deliveries();
    assert_eq!(peer_delivery_id(&rx), first_id);

    app.pending_peer_deliveries
        .get_mut(&first_id)
        .unwrap()
        .expires_at = Instant::now();
    app.flush_pending_peer_deliveries();
    let error = reply_rx
        .recv()
        .unwrap()
        .expect_err("timeout must not deliver");
    assert_eq!(error.code, Some(ipc::err_code::PEER_DELIVERY_UNCONFIRMED));

    let (retry_tx, _retry_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "retry".into(), retry_tx);
    let second_id = peer_delivery_id(&rx);
    assert_ne!(second_id, first_id, "failed send must not poison dedupe");
    app.shutdown();
}

#[test]
fn queued_flush_timeout_returns_message_to_pre_ready_queue() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender = app.ws().focused_pane_id;
    let target = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split");
    assert_eq!(
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), "keep".into())
            .unwrap(),
        ipc::PeerSendOutcome::Queued
    );
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let delivery_id = *app.pending_peer_deliveries.keys().next().unwrap();
    app.pending_peer_deliveries
        .get_mut(&delivery_id)
        .unwrap()
        .expires_at = Instant::now();
    app.flush_pending_peer_deliveries();
    assert_eq!(app.pending_peer_inbox[&target][0].body, "keep");
    assert!(!app.peer_delivery_ready.contains(&target));
    app.shutdown();
}

#[test]
fn subscriber_disconnect_never_reports_delivered() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Claude);
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(
        sender,
        &ipc::PaneRef::Id(target),
        "disconnect".into(),
        reply_tx,
    );
    let _ = peer_delivery_id(&rx);
    app.handle_peer_subscriber_gone(target);
    let error = reply_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .expect_err("disconnect must fail");
    assert_eq!(error.code, Some(ipc::err_code::PEER_DELIVERY_UNCONFIRMED));
    assert!(app.pending_peer_deliveries.is_empty());
    assert!(!app.pending_codex_peer_messages.contains_key(&target));
    app.shutdown();
}

#[test]
fn full_event_channel_never_reports_delivered_without_receipt() {
    let (mut app, sender, target, _rx) = app_with_ready_peer(PeerClientKind::Claude);
    for id in 0..256 {
        app.event_bus.emit(ipc::Event::PaneStarted {
            id,
            name: None,
            role: None,
            ts_ms: 0,
        });
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "full".into(), reply_tx);
    let delivery_id = *app.pending_peer_deliveries.keys().next().unwrap();
    app.pending_peer_deliveries
        .get_mut(&delivery_id)
        .unwrap()
        .expires_at = Instant::now();
    app.flush_pending_peer_deliveries();
    let error = reply_rx
        .recv()
        .unwrap()
        .expect_err("dropped event must not deliver");
    assert_eq!(error.code, Some(ipc::err_code::PEER_DELIVERY_UNCONFIRMED));
    app.shutdown();
}
