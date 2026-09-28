use super::super::*;
use crate::app::codex_peer::{
    codex_composer_has_draft_on_screen, normalized_codex_composer_text,
    CODEX_PEER_DRAFT_STALL_TIMEOUT, CODEX_PEER_NUDGE_COMMIT_TIMEOUT, CODEX_PEER_NUDGE_GUIDANCE,
    CODEX_PEER_NUDGE_MAX_RETRIES, PENDING_PEER_INBOX_MAX_MESSAGES,
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

fn pane_composer_chunk_width(app: &App, pane_id: usize) -> usize {
    let pane = app.ws().panes.get(&pane_id).expect("pane exists");
    let parser = pane
        .parser
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    usize::from(parser.screen().size().1.saturating_sub(2).max(1))
}

fn seed_codex_busy_composer(app: &mut App, pane_id: usize, text: &str) {
    let chunk_width = pane_composer_chunk_width(app, pane_id);
    let chars = text.chars().collect::<Vec<_>>();
    let mut screen = String::from(
        "\x1b[?25h\x1b[2J\x1b[3;1H\u{25e6} Working (1m 03s \u{2022} esc to interrupt)",
    );
    for (index, chunk) in chars.chunks(chunk_width).enumerate() {
        let row = 6 + index;
        let text = chunk.iter().collect::<String>();
        let prefix = if index == 0 { "\u{203a} " } else { "  " };
        screen.push_str(&format!("\x1b[{row};1H{prefix}{text}"));
    }
    let blank_row = 6 + chars.chunks(chunk_width).len();
    let footer_row = blank_row + 1;
    let cursor_row = blank_row - 1;
    let cursor_col = chars
        .chunks(chunk_width)
        .last()
        .map_or(3, |chunk| chunk.len() + 3);
    screen.push_str(&format!(
        "\x1b[{footer_row};1H  tab to queue message  51% context left\x1b[{cursor_row};{cursor_col}H"
    ));
    seed_pane_screen(app, pane_id, screen.as_bytes());
}

fn word_wrap_codex_composer(text: &str, width: usize) -> Vec<String> {
    assert!(width > 0);
    let mut remaining = text.chars().collect::<Vec<_>>();
    let mut lines = Vec::new();
    while remaining.len() > width {
        let split = (0..=width)
            .rev()
            .find(|&index| remaining[index].is_whitespace())
            .unwrap_or(width);
        lines.push(remaining[..split].iter().collect());
        remaining.drain(..split);
        let leading_whitespace = remaining.iter().take_while(|ch| ch.is_whitespace()).count();
        remaining.drain(..leading_whitespace);
    }
    lines.push(remaining.iter().collect());
    lines
}

fn seed_codex_busy_word_wrapped_composer(
    app: &mut App,
    pane_id: usize,
    text: &str,
    text_columns: usize,
) {
    let lines = word_wrap_codex_composer(text, text_columns);
    let mut screen = String::from(
        "\x1b[?25h\x1b[2J\x1b[3;1H\u{25e6} Working (1m 03s \u{2022} esc to interrupt)",
    );
    for (index, text) in lines.iter().enumerate() {
        let row = 6 + index;
        let prefix = if index == 0 { "\u{203a} " } else { "  " };
        screen.push_str(&format!("\x1b[{row};1H{prefix}{text}"));
    }
    let blank_row = 6 + lines.len();
    let footer_row = blank_row + 1;
    let cursor_row = blank_row - 1;
    let cursor_col = lines.last().map_or(3, |line| line.chars().count() + 3);
    screen.push_str(&format!(
        "\x1b[{footer_row};1H  tab to queue message  51% context left\x1b[{cursor_row};{cursor_col}H"
    ));
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
    // Some changed-draft fixtures include CJK text, so keep enough columns
    // for every fixture character to occupy two terminal cells.
    let chunk_width = (pane_composer_chunk_width(app, pane_id) / 2).max(1);
    let chars = text.chars().collect::<Vec<_>>();
    let mut screen = String::from("\x1b[?25h\x1b[2J");
    for (index, chunk) in chars.chunks(chunk_width).enumerate() {
        let row = 1 + index;
        let text = chunk.iter().collect::<String>();
        let prefix = if index == 0 { "\u{203a} " } else { "  " };
        screen.push_str(&format!("\x1b[{row};1H{prefix}{text}"));
    }
    let blank_row = 1 + chars.chunks(chunk_width).len();
    let footer_row = blank_row + 1;
    let cursor_row = blank_row - 1;
    let cursor_col = chars
        .chunks(chunk_width)
        .last()
        .map_or(3, |chunk| chunk.len() + 3);
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
        PendingCodexPeerDelivery::SubmitAt {
            ready_at,
            expires_at,
            ..
        } => {
            let now = Instant::now();
            *ready_at = now;
            *expires_at = now + CODEX_PEER_NUDGE_COMMIT_TIMEOUT;
        }
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
        formatted.ends_with(CODEX_PEER_NUDGE_GUIDANCE),
        "{formatted:?}"
    );
    assert!(
        formatted.contains("use send_message only when a reply or status update is needed."),
        "{formatted:?}"
    );
}

#[test]
fn codex_peer_nudge_guidance_names_the_mcp_server_and_tool() {
    assert!(CODEX_PEER_NUDGE_GUIDANCE.contains("renga-peers"));
    assert!(CODEX_PEER_NUDGE_GUIDANCE.contains("check_messages"));
}

#[test]
fn codex_peer_nudge_write_matches_the_submit_guard() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_live_ready_placeholder(&mut app, codex_id);

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "verify exact nudge text".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    let written = app
        .ws()
        .panes
        .get(&codex_id)
        .expect("Codex pane")
        .test_input();
    let expected_raw = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: None,
        from_kind: None,
    });
    assert_eq!(written, expected_raw.as_bytes());

    let expected_guard: String = expected_raw
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    match app
        .pending_codex_peer_messages
        .get(&codex_id)
        .and_then(|queue| queue.front())
    {
        Some(PendingCodexPeerDelivery::SubmitAt {
            expected_composer, ..
        }) => assert_eq!(expected_composer, &expected_guard),
        other => panic!("expected SubmitAt, got {other:?}"),
    }
    app.shutdown();
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
    app.handle_peer_subscriber_arrived(sibling_id);
    assert!(app.peer_live_subscribers.contains(&sibling_id));
    app.codex_peer_injected_composers
        .insert(sibling_id, VecDeque::from(["oldnudge".to_string()]));

    app.handle_close(&ipc::PaneRef::Id(sibling_id))
        .expect("close sibling");
    assert!(!app.pending_peer_inbox.contains_key(&sibling_id));
    assert!(!app.peer_live_subscribers.contains(&sibling_id));
    assert!(!app.codex_peer_injected_composers.contains_key(&sibling_id));
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
    app.handle_peer_subscriber_arrived(sibling_id);
    app.codex_peer_injected_composers
        .insert(sibling_id, VecDeque::from(["oldnudge".to_string()]));
    app.shutdown();
    assert!(app.pending_peer_inbox.is_empty());
    assert!(app.peer_live_subscribers.is_empty());
    assert!(app.codex_peer_injected_composers.is_empty());
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
    app.handle_peer_subscriber_arrived(sibling_id);
    app.codex_peer_injected_composers
        .insert(sibling_id, VecDeque::from(["oldnudge".to_string()]));

    let closing_tab = app.active_tab;
    app.close_tab(closing_tab);
    assert!(!app.pending_peer_inbox.contains_key(&sibling_id));
    assert!(!app.peer_live_subscribers.contains(&sibling_id));
    assert!(!app.codex_peer_injected_composers.contains_key(&sibling_id));
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
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let consumed = app.handle_key_event(esc).expect("dismiss notification");
    assert!(consumed);
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(app
        .ws()
        .panes
        .get(&sibling_id)
        .expect("pane")
        .test_input()
        .is_empty());
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "dismissing the notification should not silently queue a PTY nudge"
    );
    app.shutdown();
}

#[test]
fn focused_codex_notification_navigation_stays_visible_and_reaches_pty() {
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
    let expected_notification = app
        .visible_codex_peer_notification()
        .expect("visible notification")
        .clone();
    let expected_queue_len = app
        .pending_codex_peer_messages
        .get(&sibling_id)
        .map(VecDeque::len);
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();

    let cases = [
        (KeyCode::Up, KeyModifiers::NONE, b"\x1b[A".as_slice()),
        (KeyCode::Down, KeyModifiers::NONE, b"\x1b[B".as_slice()),
        (KeyCode::Left, KeyModifiers::NONE, b"\x1b[D".as_slice()),
        (KeyCode::Right, KeyModifiers::NONE, b"\x1b[C".as_slice()),
        (KeyCode::Home, KeyModifiers::SHIFT, b"\x1b[H".as_slice()),
        (KeyCode::End, KeyModifiers::NONE, b"\x1b[F".as_slice()),
        (KeyCode::PageUp, KeyModifiers::NONE, b"\x1b[5~".as_slice()),
        (
            KeyCode::PageDown,
            KeyModifiers::CONTROL,
            b"\x1b[6~".as_slice(),
        ),
        (KeyCode::Left, KeyModifiers::SHIFT, b"\x1b[D".as_slice()),
        (KeyCode::Left, KeyModifiers::CONTROL, b"\x1b[D".as_slice()),
    ];

    for (code, modifiers, expected_bytes) in cases {
        let before = app
            .ws()
            .panes
            .get(&sibling_id)
            .expect("pane")
            .test_input()
            .len();
        let consumed = app
            .handle_key_event(KeyEvent::new(code, modifiers))
            .expect("route navigation");

        assert!(!consumed);
        app.forward_key_to_pty(KeyEvent::new(code, modifiers))
            .expect("forward navigation to PTY");
        assert_eq!(
            app.visible_codex_peer_notification(),
            Some(&expected_notification)
        );
        assert_eq!(
            app.pending_codex_peer_messages
                .get(&sibling_id)
                .map(VecDeque::len),
            expected_queue_len
        );
        assert_eq!(
            &app.ws().panes.get(&sibling_id).expect("pane").test_input()[before..],
            expected_bytes
        );
    }
    app.shutdown();
}

#[test]
fn focused_codex_notification_alt_left_switches_tabs_without_reaching_pty() {
    let mut app = App::new(40, 80).expect("App::new");
    app.new_tab().expect("second tab");
    let notification_tab = app.active_tab;
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
        .expect("focus Codex");
    seed_codex_draft(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    let consumed = app
        .handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))
        .expect("switch tabs");

    assert!(consumed);
    assert_eq!(app.active_tab, notification_tab - 1);
    assert!(app.codex_peer_notification.is_some());
    assert!(app.workspaces[notification_tab]
        .panes
        .get(&codex_id)
        .expect("Codex pane")
        .test_input()
        .is_empty());
    app.shutdown();
}

#[test]
fn focused_codex_notification_alt_page_up_uses_scrollback_handler() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus Codex");
    seed_codex_draft(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");
    let expected_notification = app
        .visible_codex_peer_notification()
        .expect("visible notification")
        .clone();
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    let consumed = app
        .handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::ALT))
        .expect("scroll pane");

    assert!(consumed);
    assert_eq!(
        app.visible_codex_peer_notification(),
        Some(&expected_notification)
    );
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
fn focused_codex_notification_copy_mode_consumes_left() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    app.handle_focus(&ipc::PaneRef::Id(codex_id))
        .expect("focus Codex");
    seed_codex_draft(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");
    let expected_notification = app
        .visible_codex_peer_notification()
        .expect("visible notification")
        .clone();
    app.ws_mut().last_pane_rects = vec![(codex_id, Rect::new(0, 0, 40, 12))];
    app.copy_mode = Some(CopyModeState {
        pane_id: codex_id,
        cursor_row: 2,
        cursor_col: 3,
        anchor: None,
    });
    app.ws_mut()
        .panes
        .get_mut(&codex_id)
        .expect("pane")
        .clear_test_input();

    let consumed = app
        .handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .expect("move copy cursor");

    assert!(consumed);
    assert_eq!(
        app.copy_mode
            .as_ref()
            .map(|copy_mode| (copy_mode.cursor_row, copy_mode.cursor_col)),
        Some((2, 2))
    );
    assert_eq!(
        app.visible_codex_peer_notification(),
        Some(&expected_notification)
    );
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
fn focused_codex_notification_printable_requeues_and_reaches_pty() {
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
    app.ws_mut()
        .panes
        .get_mut(&sibling_id)
        .expect("pane")
        .clear_test_input();

    let consumed = app
        .handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("route printable character");

    assert!(!consumed);
    app.forward_key_to_pty(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("forward printable character to PTY");
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .and_then(|queue| queue.front()),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    assert_eq!(
        app.ws().panes.get(&sibling_id).expect("pane").test_input(),
        b"x"
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
    assert!(app.codex_peer_injected_composers.contains_key(&sibling_id));

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
    assert!(!app.codex_peer_injected_composers.contains_key(&sibling_id));
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

// Codex v0.158.0 idle screen as captured from a live 58x114 pane
// (renga-6fx): prompt row, blank row, model row, then a hint row carrying
// `? for shortcuts` and a right-aligned `⚠ 2 warnings · f2 to view`.
const CODEX_0158_IDLE_FOOTER: &[u8] = b"\x1b[?25h\x1b[2J\x1b[55;1H\x1b[1m\xE2\x80\xBA\x1b[22m \x1b[2mAsk Codex to do anything\x1b[22m\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n  \x1b[1m?\x1b[22m for shortcuts\x1b[K\x1b[57;3H\x1b[38;5;3mGPT-6-Astra high \x1b[m\xC2\xB7 \x1b[38;5;2m~\\Develop\\renga\x1b[K\x1b[58;89H\xE2\x9A\xA0 \x1b[38;2;196;167;103m2 warnings\x1b[m\x1b[1C\xC2\xB7\x1b[1m\x1b[1Cf2\x1b[22m\x1b[1Cto\x1b[1Cview\x1b[55;3H";

fn codex_0158_screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser
}

#[test]
fn codex_0158_two_row_footer_is_recognized_in_every_hint_shape() {
    let idle = codex_0158_screen(58, 114, CODEX_0158_IDLE_FOOTER);
    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(idle.screen()),
        Some(true)
    );
    assert_eq!(
        codex_composer_has_draft_on_screen(idle.screen()),
        Some(false)
    );

    // Hint rows: shortcuts only, warnings only (a draft hides the shortcuts
    // hint), a singular warning, and the busy model row with its spinner.
    for (model, hint) in [
        (
            "GPT-6-Astra high \u{b7} ~\\Develop\\renga",
            "  ? for shortcuts",
        ),
        (
            "GPT-6-Astra high \u{b7} ~\\Develop\\renga",
            "        \u{26a0} 2 warnings \u{b7} f2 to view",
        ),
        (
            "GPT-6-Astra default \u{b7} ~\\Develop\\renga",
            "  ? for shortcuts   \u{26a0} 1 warning \u{b7} f2 to view",
        ),
        (
            "GPT-6-Astra xhigh \u{b7} ~\\Develop\\renga \u{b7} \u{283c}",
            "  ? for shortcuts   \u{26a0} 12 warnings \u{b7} f2 to view",
        ),
    ] {
        let screen = format!(
            "\x1b[?25h\x1b[2J\x1b[10;1H\u{203a} \x1b[2mAsk Codex to do anything\x1b[22m\x1b[12;1H{model}\x1b[13;1H{hint}\x1b[10;3H"
        );
        let parser = codex_0158_screen(20, 114, screen.as_bytes());
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(true),
            "model {model:?} hint {hint:?}"
        );
    }
}

#[test]
fn codex_0158_rows_below_the_model_row_are_accepted_whatever_they_hold() {
    for below in [
        "  reading src/app.rs",
        "  tab to queue message",
        "  \u{26a0} 2 warnings \u{b7} f3 to view",
        "  \u{65b0}\u{3057}\u{3044}\u{8868}\u{793a} \u{2728}",
        "  ? for shortcuts\x1b[14;1Hsecond hint row\x1b[15;1Hthird hint row",
        "\x1b[14;1H  a hint after an extra blank row",
    ] {
        let screen = format!(
            "\x1b[?25h\x1b[2J\x1b[10;1H\u{203a} \x1b[2mAsk Codex to do anything\x1b[22m\x1b[12;1HGPT-6-Astra high \u{b7} cwd\x1b[13;1H{below}\x1b[10;3H"
        );
        let parser = codex_0158_screen(20, 40, screen.as_bytes());
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(true),
            "rows below the model row: {below:?}"
        );
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false),
            "rows below the model row: {below:?}"
        );
    }
}

#[test]
fn codex_0158_draft_stays_protected_whatever_is_below_the_model_row() {
    for below in [
        "  ? for shortcuts",
        "  anything at all",
        "  tab to queue message",
    ] {
        let screen = format!(
            "\x1b[?25h\x1b[2J\x1b[10;1H\u{203a} please refactor the parser\x1b[12;1HGPT-6-Astra high \u{b7} cwd\x1b[13;1H{below}\x1b[10;29H"
        );
        let parser = codex_0158_screen(20, 114, screen.as_bytes());
        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(true),
            "a typed draft must stay protected: {below:?}"
        );
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(false),
            "a typed draft must not accept a nudge: {below:?}"
        );
    }
}

#[test]
fn codex_0158_draft_line_that_looks_like_a_model_row_never_receives_a_nudge() {
    // Drafts that open with an empty line and a blank line, whose next line
    // reads like a model row, above the real footer (0.158 two rows, 0.147
    // one row). Reproduced by review of 2946bfc as an injection into a draft.
    for (draft_line, footer) in [
        (
            "high \u{b7} x",
            "\x1b[14;1Hgpt-5.5 high \u{b7} ~\\renga\x1b[15;1H  ? for shortcuts",
        ),
        ("high \u{b7} x", "\x1b[14;1Hgpt-5.5 high \u{b7} ~\\renga"),
        (
            "please follow \u{b7} the plan",
            "\x1b[14;1HGPT-6-Astra high \u{b7} cwd\x1b[15;1H  anything",
        ),
    ] {
        let cursor_col = draft_line.chars().count() + 1;
        let screen = format!(
            "\x1b[?25h\x1b[2J\x1b[10;1H\u{203a} \x1b[12;1H{draft_line}{footer}\x1b[12;{cursor_col}H"
        );
        let parser = codex_0158_screen(20, 60, screen.as_bytes());
        assert_ne!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(true),
            "draft line {draft_line:?} with footer {footer:?}"
        );
        assert_ne!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false),
            "draft line {draft_line:?} with footer {footer:?}"
        );
    }
}

#[test]
fn codex_0158_footer_without_a_model_row_stays_unrecognized() {
    let parser = codex_0158_screen(
        20,
        114,
        "\x1b[?25h\x1b[2J\x1b[10;1H\u{203a} \x1b[2mAsk Codex to do anything\x1b[22m\x1b[12;1H  ? for shortcuts\x1b[13;1H  reading src/app.rs\x1b[10;3H"
            .as_bytes(),
    );
    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
        None
    );
}

#[test]
fn codex_0158_idle_pane_receives_the_peer_nudge() {
    let mut app = App::new(58, 230).expect("App::new");
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
    // The captured bytes, moved to rows that fit the split pane.
    let idle = String::from_utf8(CODEX_0158_IDLE_FOOTER.to_vec())
        .expect("utf8 fixture")
        .replace("\x1b[55;1H", "\x1b[10;1H")
        .replace("\x1b[57;3H", "\x1b[12;3H")
        .replace("\x1b[58;89H", "\x1b[13;60H")
        .replace("\x1b[55;3H", "\x1b[10;3H");
    seed_pane_screen(&mut app, codex_id, idle.as_bytes());
    {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            codex_prompt_allows_peer_nudge_on_screen(parser.screen()),
            Some(true)
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
        "codex 0.158".to_string(),
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
        "a Codex 0.158 idle composer must receive the peer nudge"
    );
    app.shutdown();
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
    assert!(app.codex_peer_injected_composers.contains_key(&sibling_id));

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
    assert!(!app.codex_peer_injected_composers.contains_key(&sibling_id));
    app.shutdown();
}

#[test]
fn named_sender_nudge_at_field_width_reaches_busy_queue_footer() {
    let mut app = App::new(40, 138).expect("App::new");
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
    app.ws_mut()
        .pane_names
        .insert("impl-h25-pot-authority".to_string(), sender_id);
    app.peer_client_kinds
        .insert(sender_id, PeerClientKind::Claude);
    app.peer_client_kinds
        .insert(codex_id, PeerClientKind::Codex);
    app.peer_delivery_ready.insert(codex_id);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");

    let pane_columns = {
        let pane = app.ws().panes.get(&codex_id).expect("pane");
        let parser = pane
            .parser
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        usize::from(parser.screen().size().1)
    };
    assert_eq!(pane_columns.saturating_sub(3), 53);
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "field-width queue footer".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();

    let expected = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: sender_id,
        from_name: Some("impl-h25-pot-authority".to_string()),
        from_kind: Some(PeerClientKind::Claude),
    });
    let header = expected
        .strip_suffix(CODEX_PEER_NUDGE_GUIDANCE)
        .expect("shared guidance suffix")
        .trim_end_matches(". ");
    assert_eq!(header.chars().count(), 62);
    seed_codex_busy_word_wrapped_composer(&mut app, codex_id, &expected, 53);
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
        "the field-width busy footer must remain inside the measured scan range"
    );
    assert!(!app.pending_codex_peer_messages.contains_key(&codex_id));
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
fn clipped_known_busy_status_without_ellipsis_never_falls_through_to_enter() {
    let status = "◦ Working (19s • esc to interr";
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    let empty_screen = format!(
        "\x1b[?25h\x1b[2J\x1b[H{status}\x1b[4;1H\u{203a} \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1H  gpt-5.6-sol medium \u{b7} cwd\x1b[4;3H"
    );
    seed_pane_screen(&mut app, codex_id, empty_screen.as_bytes());
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "clipped known status".to_string(),
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
        "a recognized busy status must block Enter even without ellipsis"
    );
    app.shutdown();
}

#[test]
fn transcript_prefix_before_working_status_is_not_native_queue_busy() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[Hthe agent was Working (12s \xE2\x80\xA2 esc to interrupt) at that point.\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "anchored transcript".to_string(),
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
fn truncated_tool_file_count_is_not_interrupt_status() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xA2 Explored(12 files) and summarised the modu\xE2\x80\xA6\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "tool count transcript".to_string(),
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
fn working_file_count_transcript_is_not_native_queue_busy() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HWorking (2 files) were left over\xE2\x80\xA6\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "working file transcript".to_string(),
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
fn working_duration_prefix_followed_by_prose_is_not_native_queue_busy() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[HWorking (2m ago) was the last note in the log.\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "duration prefix transcript".to_string(),
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
fn truncated_unknown_status_blocks_idle_and_commit_paths() {
    let status = "◦ Reticulating (12s • esc to inte…";
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    let empty_screen = format!(
        "\x1b[?25h\x1b[2J\x1b[H{status}\x1b[4;1H\u{203a} \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1H  gpt-5.6-sol medium \u{b7} cwd\x1b[4;3H"
    );
    seed_pane_screen(&mut app, codex_id, empty_screen.as_bytes());
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "unknown truncated status".to_string(),
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

    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_codex_busy_placeholder(&mut app, codex_id);
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "status changed before commit".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
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
        "an ellipsis-truncated unknown status must not select Enter"
    );
    assert!(matches!(
        app.pending_codex_peer_messages
            .get(&codex_id)
            .and_then(|q| q.front()),
        Some(PendingCodexPeerDelivery::QueueAt { .. })
    ));
    app.shutdown();
}

#[test]
fn truncated_unknown_status_without_numeric_elapsed_keeps_idle_path() {
    let (mut app, sender_id, codex_id) = setup_unfocused_registered_codex();
    seed_pane_screen(
        &mut app,
        codex_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x97\xA6 Reticulating (see below)\xE2\x80\xA6\x1b[4;1H\xE2\x80\xBA \x1b[2mAsk Codex to do anything\x1b[22m\x1b[6;1Hgpt-5.6-sol medium \xC2\xB7 cwd\x1b[4;3H",
    );

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_id),
        "nonnumeric unknown status".to_string(),
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
    seed_codex_idle_composer(&mut app, sibling_id, &expected);
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
    assert!(app.codex_peer_injected_composers.contains_key(&sibling_id));
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
        Some(PendingCodexPeerDelivery::Draft { .. })
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
fn current_codex_placeholder_without_dim_is_not_a_draft() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA Ask Codex to do anything\x1b[3;1H  gpt-5.6-sol medium \xC2\xB7 cwd\x1b[3;20H",
    );

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(false)
    );
}

#[test]
fn similarly_worded_codex_draft_without_dim_remains_protected() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA Ask Codex to refactor foo\x1b[3;1H  gpt-5.6-sol medium \xC2\xB7 cwd\x1b[3;20H",
    );

    assert_eq!(
        codex_composer_has_draft_on_screen(parser.screen()),
        Some(true)
    );
}

#[test]
fn codex_rotating_placeholders_at_editable_start_are_not_drafts() {
    for placeholder in [
        "Ask Codex to do anything",
        "Ask a follow-up question",
        "Explain this codebase",
        "Summarize recent commits",
        "Implement {feature}",
        "Find and fix a bug in @filename",
        "Write tests for @filename",
        "Improve documentation in @filename",
    ] {
        let mut parser = vt100::Parser::new(40, 80, 0);
        parser.process(format!("\x1b[?25h\x1b[2J\x1b[H\u{203a} {placeholder}\x1b[1;3H").as_bytes());

        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(false),
            "expected exact placeholder to be treated as empty: {placeholder:?}"
        );
    }
}

#[test]
fn codex_placeholder_prefixes_with_user_text_remain_drafts_at_editable_start() {
    for draft in [
        "Explain this codebase to me",
        "Write tests for @filename now",
    ] {
        let mut parser = vt100::Parser::new(40, 80, 0);
        parser.process(format!("\x1b[?25h\x1b[2J\x1b[H\u{203a} {draft}\x1b[1;3H").as_bytes());

        assert_eq!(
            codex_composer_has_draft_on_screen(parser.screen()),
            Some(true),
            "expected placeholder-prefixed user input to remain a draft: {draft:?}"
        );
    }
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
fn codex_composer_text_at_editable_start_is_still_a_draft() {
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA retained nudge\x1b[1;3H");

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
    assert_eq!(
        app.handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "queued".into()),
        Ok(ipc::PeerSendOutcome::Queued)
    );
    let peers = app.handle_peer_list(sender_id).expect("peer list");
    assert_eq!(peers.len(), 1, "expected one sibling, got {peers:?}");
    assert_eq!(peers[0].id, sibling_id);
    assert_eq!(peers[0].name.as_deref(), Some("sibling"));
    assert_eq!(peers[0].role.as_deref(), Some("worker"));
    assert_eq!(peers[0].pending_peer_messages, 1);
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
        app.handle_peer_subscriber_arrived(pane_id);
        assert!(app.peer_delivery_ready.contains(&pane_id));
        assert!(app.peer_live_subscribers.contains(&pane_id));

        app.handle_peer_subscriber_gone(pane_id, "event_bus_closed");

        assert!(!app.peer_delivery_ready.contains(&pane_id));
        assert!(!app.peer_live_subscribers.contains(&pane_id));
        assert_eq!(app.peer_client_kinds.get(&pane_id), Some(&kind));
        app.shutdown();
    }
}

#[test]
fn codex_kind_can_change_to_claude_without_a_live_subscriber() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    app.handle_peer_register_client(pane_id, PeerClientKind::Codex)
        .expect("register Codex before subscribe");
    assert!(!app.peer_live_subscribers.contains(&pane_id));
    app.handle_peer_register_client(pane_id, PeerClientKind::Claude)
        .expect("replace a peer that never subscribed");

    assert_eq!(
        app.peer_client_kinds.get(&pane_id),
        Some(&PeerClientKind::Claude)
    );
    app.shutdown();
}

#[test]
fn codex_kind_can_change_to_claude_after_the_last_subscriber_leaves() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.handle_peer_register_client(pane_id, PeerClientKind::Codex)
        .expect("register Codex");
    app.handle_peer_subscriber_arrived(pane_id);
    app.handle_peer_subscriber_gone(pane_id, "event_bus_closed");

    app.handle_peer_register_client(pane_id, PeerClientKind::Claude)
        .expect("register replacement Claude");

    assert_eq!(
        app.peer_client_kinds.get(&pane_id),
        Some(&PeerClientKind::Claude)
    );
    app.shutdown();
}

#[test]
fn claude_kind_can_change_to_codex_while_subscribed() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.handle_peer_register_client(pane_id, PeerClientKind::Claude)
        .expect("register Claude");
    app.handle_peer_subscriber_arrived(pane_id);

    app.handle_peer_register_client(pane_id, PeerClientKind::Codex)
        .expect("upgrade to Codex");

    assert_eq!(
        app.peer_client_kinds.get(&pane_id),
        Some(&PeerClientKind::Codex)
    );
    app.shutdown();
}

#[test]
fn live_but_not_ready_codex_subscriber_refuses_claude_downgrade() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.handle_peer_register_client(pane_id, PeerClientKind::Codex)
        .expect("register Codex");
    app.handle_peer_subscriber_arrived(pane_id);
    assert!(!app.peer_delivery_ready.contains(&pane_id));

    app.handle_peer_register_client(pane_id, PeerClientKind::Claude)
        .expect("nested Claude registration is handled");

    assert_eq!(
        app.peer_client_kinds.get(&pane_id),
        Some(&PeerClientKind::Codex)
    );
    assert!(!app.peer_delivery_ready.contains(&pane_id));
    app.shutdown();
}

#[test]
fn refused_claude_readiness_flush_attaches_codex_nudge() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let target_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split target");
    app.handle_peer_register_client(target_id, PeerClientKind::Codex)
        .expect("register Codex");
    app.handle_peer_subscriber_arrived(target_id);
    assert_eq!(
        app.handle_peer_send(sender_id, &ipc::PaneRef::Id(target_id), "queued".into())
            .expect("queue before readiness"),
        ipc::PeerSendOutcome::Queued
    );

    app.handle_peer_set_ready(target_id, PeerClientKind::Claude, true)
        .expect("refused Claude kind still flushes readiness");

    let flushed = app
        .pending_peer_deliveries
        .values()
        .find(|pending| pending.target_pane == target_id)
        .expect("queued message starts delivery");
    assert!(flushed.nudge.is_some());
    app.shutdown();
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

fn peer_delivery_ids(rx: &std::sync::mpsc::Receiver<ipc::Event>, count: usize) -> Vec<u64> {
    (0..count).map(|_| peer_delivery_id(rx)).collect()
}

#[test]
fn bulk_flush_retries_only_the_oldest_unacknowledged_delivery() {
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
        .expect("split");
    for index in 0..8 {
        assert_eq!(
            app.handle_peer_send(sender, &ipc::PaneRef::Id(target), format!("queued-{index}"),)
                .unwrap(),
            ipc::PeerSendOutcome::Queued
        );
    }
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 8);
    for pending in app.pending_peer_deliveries.values_mut() {
        pending.next_retry_at = Instant::now();
    }

    app.flush_pending_peer_deliveries();

    assert_eq!(peer_delivery_id(&rx), delivery_ids[0]);
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })));

    app.handle_peer_inbox_ack(target, delivery_ids[0]).unwrap();
    app.pending_peer_deliveries
        .get_mut(&delivery_ids[1])
        .unwrap()
        .next_retry_at = Instant::now();
    app.flush_pending_peer_deliveries();
    assert_eq!(peer_delivery_id(&rx), delivery_ids[1]);
    app.shutdown();
}

#[test]
fn bulk_flush_followers_do_not_expire_while_waiting_for_the_head() {
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
        .expect("split");
    for index in 0..3 {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), format!("queued-{index}"))
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 3);
    for delivery_id in &delivery_ids[1..] {
        app.pending_peer_deliveries
            .get_mut(delivery_id)
            .unwrap()
            .expires_at = Instant::now();
    }

    app.flush_pending_peer_deliveries();

    assert_eq!(app.pending_peer_deliveries.len(), 3);
    assert!(app.peer_delivery_ready.contains(&target));
    app.handle_peer_inbox_ack(target, delivery_ids[0]).unwrap();
    assert!(app.pending_peer_deliveries[&delivery_ids[1]].expires_at > Instant::now());
    app.shutdown();
}

#[test]
fn expired_bulk_flush_head_restores_the_whole_pane_fifo() {
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
    for index in 0..3 {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), format!("queued-{index}"))
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let head = *app.pending_peer_deliveries.keys().min().unwrap();
    app.pending_peer_deliveries
        .get_mut(&head)
        .unwrap()
        .expires_at = Instant::now();

    app.flush_pending_peer_deliveries();

    assert!(app.pending_peer_deliveries.is_empty());
    assert_eq!(
        app.pending_peer_inbox[&target]
            .iter()
            .map(|message| message.body.as_str())
            .collect::<Vec<_>>(),
        vec!["queued-0", "queued-1", "queued-2"]
    );
    assert!(!app.peer_delivery_ready.contains(&target));
    app.shutdown();
}

#[test]
fn late_follower_ack_removes_requeued_delivery_before_ready_flush() {
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
        .expect("split");
    for body in ["head", "late-acked-follower"] {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), body.into())
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 2);
    app.pending_peer_deliveries
        .get_mut(&delivery_ids[0])
        .unwrap()
        .expires_at = Instant::now();
    app.flush_pending_peer_deliveries();

    app.handle_peer_inbox_ack(target, delivery_ids[1]).unwrap();
    assert_eq!(app.pending_peer_inbox[&target].len(), 1);
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();

    assert_eq!(peer_delivery_id(&rx), delivery_ids[0]);
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })));
    app.shutdown();
}

#[test]
fn late_requeued_ack_drops_stale_codex_nudge_after_kind_changes_to_claude() {
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
        .expect("split");
    for body in ["head", "codex-tail"] {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), body.into())
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Codex, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 2);
    app.pending_peer_deliveries
        .get_mut(&delivery_ids[0])
        .unwrap()
        .expires_at = Instant::now();
    app.flush_pending_peer_deliveries();
    app.handle_peer_register_client(target, PeerClientKind::Claude)
        .unwrap();
    app.handle_focus(&ipc::PaneRef::Id(sender)).unwrap();

    app.handle_peer_inbox_ack(target, delivery_ids[1]).unwrap();

    assert!(!app.pending_codex_peer_messages.contains_key(&target));
    app.shutdown();
}

#[test]
fn codex_reflush_assigns_the_nudge_only_to_the_current_fifo_tail() {
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
        .expect("split");
    for body in ["old-head", "old-tail"] {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), body.into())
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Codex, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 2);
    app.pending_peer_deliveries
        .get_mut(&delivery_ids[0])
        .unwrap()
        .expires_at = Instant::now();
    app.flush_pending_peer_deliveries();
    app.handle_peer_send(sender, &ipc::PaneRef::Id(target), "new-tail".into())
        .unwrap();

    app.handle_peer_set_ready(target, PeerClientKind::Codex, true)
        .unwrap();

    let nudged = app
        .pending_peer_deliveries
        .values()
        .filter(|pending| pending.nudge.is_some())
        .collect::<Vec<_>>();
    assert_eq!(nudged.len(), 1);
    assert_eq!(nudged[0].message.body, "new-tail");
    app.shutdown();
}

#[test]
fn expired_flush_head_leaves_direct_follower_pending_for_its_own_deadline() {
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
        .expect("split");
    for body in ["head", "direct-follower"] {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), body.into())
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 2);
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(
        sender,
        &ipc::PaneRef::Id(target),
        "direct-follower".into(),
        reply_tx,
    );
    app.pending_peer_deliveries
        .get_mut(&delivery_ids[0])
        .unwrap()
        .expires_at = Instant::now();

    app.flush_pending_peer_deliveries();

    assert!(app.pending_peer_deliveries.contains_key(&delivery_ids[1]));
    assert!(reply_rx.recv_timeout(Duration::from_millis(10)).is_err());
    assert!(!app.peer_delivery_ready.contains(&target));
    app.pending_peer_deliveries
        .get_mut(&delivery_ids[1])
        .unwrap()
        .next_retry_at = Instant::now();
    app.flush_pending_peer_deliveries();
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })));
    app.handle_peer_inbox_ack(target, delivery_ids[1]).unwrap();
    assert_eq!(
        reply_rx.recv().unwrap().unwrap(),
        ipc::PeerSendOutcome::Delivered
    );
    app.shutdown();
}

#[test]
fn direct_send_joining_a_flush_follower_keeps_its_original_deadline() {
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
        .expect("split");
    for body in ["head", "joined"] {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), body.into())
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 2);
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "joined".into(), reply_tx);
    let direct_deadline = app.pending_peer_deliveries[&delivery_ids[1]].expires_at;

    app.handle_peer_inbox_ack(target, delivery_ids[0]).unwrap();

    assert_eq!(
        app.pending_peer_deliveries[&delivery_ids[1]].expires_at,
        direct_deadline
    );
    app.pending_peer_deliveries
        .get_mut(&delivery_ids[1])
        .unwrap()
        .expires_at = Instant::now();
    app.flush_pending_peer_deliveries();
    assert_eq!(
        reply_rx.recv().unwrap().unwrap_err().code,
        Some(ipc::err_code::PEER_DELIVERY_UNCONFIRMED)
    );
    app.shutdown();
}

#[test]
fn bulk_flush_of_128_deliveries_does_not_expire_followers_as_acks_progress() {
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
        .expect("split");
    for index in 0..128 {
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), format!("queued-{index}"))
            .unwrap();
    }
    app.handle_peer_set_ready(target, PeerClientKind::Claude, true)
        .unwrap();
    let delivery_ids = peer_delivery_ids(&rx, 128);
    for delivery_id in &delivery_ids[1..] {
        app.pending_peer_deliveries
            .get_mut(delivery_id)
            .unwrap()
            .expires_at = Instant::now();
    }

    for delivery_id in delivery_ids {
        app.handle_peer_inbox_ack(target, delivery_id).unwrap();
        app.flush_pending_peer_deliveries();
    }

    assert!(app.pending_peer_deliveries.is_empty());
    assert!(app.peer_delivery_ready.contains(&target));
    app.shutdown();
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
    app.handle_peer_subscriber_gone(target, "event_bus_closed");
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

#[test]
fn pending_peer_message_count_combines_inbox_and_nudges_per_pane() {
    let mut app = App::new(40, 120).expect("App::new");
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
        .expect("split target");
    let other = app
        .handle_split(
            &ipc::PaneRef::Id(target),
            ipc::Direction::Horizontal,
            None,
            None,
            None,
            None,
        )
        .expect("split other");
    assert_eq!(
        app.handle_peer_send(sender, &ipc::PaneRef::Id(target), "queued".into()),
        Ok(ipc::PeerSendOutcome::Queued)
    );
    app.pending_codex_peer_messages.insert(
        target,
        VecDeque::from([PendingCodexPeerDelivery::Draft {
            message: PendingCodexPeerMessage {
                from_pane: sender,
                from_name: None,
                from_kind: None,
            },
            retries_remaining: 0,
            stalled_since: Instant::now(),
            delivery_sequence: None,
        }]),
    );

    assert_eq!(app.pending_peer_message_count(target), 2);
    assert_eq!(app.pending_peer_message_count(other), 0);
    app.shutdown();
}

fn retain_one_codex_handover() -> (
    App,
    usize,
    usize,
    u64,
    std::sync::mpsc::Receiver<ipc::Event>,
) {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Codex);
    app.handle_peer_set_ready(sender, PeerClientKind::Claude, true)
        .unwrap();
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(
        sender,
        &ipc::PaneRef::Id(target),
        "remember me".into(),
        reply_tx,
    );
    let delivery_id = peer_delivery_id(&rx);
    app.handle_peer_inbox_ack(target, delivery_id).unwrap();
    assert_eq!(
        reply_rx.recv().unwrap().unwrap(),
        ipc::PeerSendOutcome::Delivered
    );
    assert_eq!(app.peer_handovers[&target].len(), 1);
    while rx.try_recv().is_ok() {}
    (app, sender, target, delivery_id, rx)
}

fn assert_one_loss_notice(
    rx: &std::sync::mpsc::Receiver<ipc::Event>,
    delivery_id: u64,
    sender: usize,
    target: usize,
    reason: &str,
) {
    let events: Vec<_> = rx.try_iter().collect();
    let losses = events
        .iter()
        .filter(|event| matches!(
            event,
            ipc::Event::PeerMessageLost {
                delivery_id: id,
                target_pane,
                from_pane,
                reason: event_reason,
                ..
            } if *id == delivery_id && *target_pane == target && *from_pane == sender && event_reason == reason
        ))
        .count();
    assert_eq!(losses, 1);
    let notices: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ipc::Event::PeerInbox {
                target_pane,
                from_pane,
                from_name,
                body,
                ..
            } if *target_pane == sender && *from_pane == target => {
                Some((from_name.as_deref(), body.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, Some("renga"));
    assert!(notices[0].1.contains(&format!("delivery {delivery_id}")));
    assert!(notices[0].1.contains(reason));
    assert!(notices[0].1.contains("remember me"));
}

#[test]
fn subscriber_gone_timeout_reports_each_unconsumed_codex_handover() {
    let (mut app, sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_subscriber_gone(target, "event_bus_closed");
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    app.peer_handover_disconnect_deadlines
        .insert(target, Instant::now());
    app.flush_pending_peer_deliveries();
    assert_one_loss_notice(&rx, delivery_id, sender, target, "subscriber_gone_timeout");
    assert!(!app.peer_handovers.contains_key(&target));
    app.shutdown();
}

#[test]
fn expired_submit_at_stays_counted_and_is_requeued_on_flush() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    app.pending_codex_peer_messages.insert(
        pane_id,
        VecDeque::from([PendingCodexPeerDelivery::SubmitAt {
            message: PendingCodexPeerMessage {
                from_pane: 999,
                from_name: None,
                from_kind: Some(PeerClientKind::Claude),
            },
            retries_remaining: CODEX_PEER_NUDGE_MAX_RETRIES,
            created_at: Instant::now(),
            observed_prefix_len: 0,
            ready_at: Instant::now(),
            expires_at: Instant::now(),
            expected_composer: "will-not-match".into(),
            expected_composer_raw: None,
            delivery_sequence: None,
        }]),
    );

    assert_eq!(app.pending_peer_message_count(pane_id), 1);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.pending_peer_message_count(pane_id), 1);
    assert!(matches!(app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { retries_remaining, .. })
        if *retries_remaining == CODEX_PEER_NUDGE_MAX_RETRIES - 1));
    app.shutdown();
}

#[test]
fn empty_reconcile_after_reregister_reports_each_unconsumed_codex_handover() {
    let (mut app, sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_subscriber_gone(target, "event_bus_closed");
    app.handle_peer_register_client(target, PeerClientKind::Codex)
        .unwrap();
    app.handle_peer_inbox_reconcile(target, &[], &[], 0, 0)
        .unwrap();
    assert_one_loss_notice(&rx, delivery_id, sender, target, "peer_restarted");
    app.shutdown();
}

#[test]
fn refused_claude_register_preserves_codex_handover_generation_and_ledger() {
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
        .expect("split target");
    app.handle_peer_register_client(target, PeerClientKind::Codex)
        .expect("register Codex");
    app.handle_peer_subscriber_arrived(target);
    let generation = app.peer_handover_generations[&target];
    let message = PendingPeerInboxMessage {
        from_pane: sender,
        from_name: None,
        from_kind: None,
        body: "arrived after reconcile snapshot".into(),
        ts_ms: 0,
        debug_peer_inbox_sequence: None,
        requeued_delivery_id: None,
        requeued_nudge: None,
        system_generated: false,
    };
    app.track_peer_handover(target, 9999, &message);

    app.handle_peer_register_client(target, PeerClientKind::Claude)
        .expect("refuse nested Claude registration");
    app.handle_peer_inbox_reconcile(target, &[], &[], 0, 0)
        .expect("reconcile Codex snapshot");

    assert_eq!(app.peer_handover_generations[&target], generation);
    assert!(app.peer_handovers[&target]
        .iter()
        .any(|entry| entry.delivery_id == 9999));
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    app.shutdown();
}

#[test]
fn refused_claude_register_preserves_handover_disconnect_deadline() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.handle_peer_register_client(pane_id, PeerClientKind::Codex)
        .expect("register Codex");
    app.handle_peer_subscriber_arrived(pane_id);
    let deadline = Instant::now() + Duration::from_secs(30);
    app.peer_handover_disconnect_deadlines
        .insert(pane_id, deadline);

    app.handle_peer_register_client(pane_id, PeerClientKind::Claude)
        .expect("refuse nested Claude registration");

    assert_eq!(
        app.peer_handover_disconnect_deadlines.get(&pane_id),
        Some(&deadline)
    );
    app.shutdown();
}

#[test]
fn reconnect_reconcile_with_held_message_sends_no_loss_notice() {
    let (mut app, _sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_subscriber_gone(target, "event_bus_closed");
    app.handle_peer_register_client(target, PeerClientKind::Codex)
        .unwrap();
    app.handle_peer_inbox_reconcile(target, &[delivery_id], &[], 0, 0)
        .unwrap();
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    assert_eq!(app.peer_handovers[&target].len(), 1);
    assert!(!app.peer_handover_disconnect_deadlines.contains_key(&target));
    app.shutdown();
}

#[test]
fn reconnect_reconcile_with_unreported_consumed_sends_no_loss_notice() {
    let (mut app, _sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_subscriber_gone(target, "event_bus_closed");
    app.handle_peer_register_client(target, PeerClientKind::Codex)
        .unwrap();
    app.handle_peer_inbox_reconcile(target, &[], &[delivery_id], 0, 0)
        .unwrap();
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    assert!(!app.peer_handovers.contains_key(&target));
    app.shutdown();
}

#[test]
fn reconcile_overflow_retains_unmatched_handover_without_a_loss_notice() {
    let (mut app, _sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_register_client(target, PeerClientKind::Codex)
        .unwrap();
    app.handle_peer_inbox_reconcile(target, &[], &[], 1, 0)
        .unwrap();

    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    assert!(app.peer_handovers[&target]
        .iter()
        .any(|entry| entry.delivery_id == delivery_id));
    app.shutdown();
}

#[test]
fn reconcile_does_not_classify_current_generation_delivery_as_lost() {
    let (mut app, sender, target, _delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_register_client(target, PeerClientKind::Codex)
        .unwrap();
    let message = PendingPeerInboxMessage {
        from_pane: sender,
        from_name: None,
        from_kind: None,
        body: "new generation".into(),
        ts_ms: 0,
        debug_peer_inbox_sequence: None,
        requeued_delivery_id: None,
        requeued_nudge: None,
        system_generated: false,
    };
    app.track_peer_handover(target, 9999, &message);
    app.handle_peer_inbox_reconcile(target, &[], &[], 0, 0)
        .unwrap();
    let notices = rx
        .try_iter()
        .filter(|event| {
            matches!(
                event,
                ipc::Event::PeerMessageLost {
                    delivery_id: 9999,
                    ..
                }
            )
        })
        .count();
    assert_eq!(notices, 0);
    assert!(app.peer_handovers[&target]
        .iter()
        .any(|entry| entry.delivery_id == 9999));
    app.shutdown();
}

#[test]
fn reregister_cancels_disconnect_deadline_even_without_reconcile() {
    let (mut app, _sender, target, _delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_subscriber_gone(target, "event_bus_closed");
    assert!(app.peer_handover_disconnect_deadlines.contains_key(&target));
    app.handle_peer_register_client(target, PeerClientKind::Codex)
        .unwrap();
    assert!(!app.peer_handover_disconnect_deadlines.contains_key(&target));
    app.flush_peer_handover_disconnect_timeouts();
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    app.shutdown();
}

#[test]
fn pane_close_reports_each_unconsumed_codex_handover() {
    let (mut app, sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.handle_close(&ipc::PaneRef::Id(target)).unwrap();
    assert_one_loss_notice(&rx, delivery_id, sender, target, "pane_closed");
    app.shutdown();
}

#[test]
fn pane_close_removes_unconfirmed_delivery_instead_of_requeueing_it() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Codex);
    app.handle_peer_set_ready(sender, PeerClientKind::Claude, true)
        .unwrap();
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(
        sender,
        &ipc::PaneRef::Id(target),
        "not yet receipted".into(),
        reply_tx,
    );
    let delivery_id = peer_delivery_id(&rx);
    app.handle_close(&ipc::PaneRef::Id(target)).unwrap();

    assert_eq!(
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .code,
        Some(ipc::err_code::PANE_VANISHED)
    );
    assert!(!app.pending_peer_deliveries.contains_key(&delivery_id));
    assert!(!app.pending_peer_inbox.contains_key(&target));
    let events: Vec<_> = rx.try_iter().collect();
    assert!(events.iter().any(|event| matches!(
        event,
        ipc::Event::PeerMessageLost { delivery_id: id, .. } if *id == delivery_id
    )));
    app.shutdown();
}

#[test]
fn pane_close_does_not_report_unconfirmed_push_delivery_as_lost() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Claude);
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(
        sender,
        &ipc::PaneRef::Id(target),
        "push not yet receipted".into(),
        reply_tx,
    );
    let delivery_id = peer_delivery_id(&rx);
    app.handle_close(&ipc::PaneRef::Id(target)).unwrap();

    assert_eq!(
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .code,
        Some(ipc::err_code::PANE_VANISHED)
    );
    assert!(!app.pending_peer_deliveries.contains_key(&delivery_id));
    assert!(rx.try_iter().all(|event| !matches!(
        event,
        ipc::Event::PeerMessageLost {
            delivery_id: id,
            ..
        } if id == delivery_id
    )));
    app.shutdown();
}

#[test]
fn consumed_codex_handover_does_not_report_loss() {
    let (mut app, _sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_inbox_consumed(target, delivery_id).unwrap();
    app.handle_peer_subscriber_gone(target, "event_bus_closed");
    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    app.shutdown();
}

#[test]
fn consumed_before_async_receipt_leaves_no_phantom_handover() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Codex);
    let (reply_tx, reply_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "race".into(), reply_tx);
    let delivery_id = peer_delivery_id(&rx);

    app.handle_peer_inbox_consumed(target, delivery_id).unwrap();
    assert_eq!(
        app.peer_handover_consumed_tombstones[&target].front(),
        Some(&delivery_id)
    );
    app.handle_peer_inbox_ack(target, delivery_id).unwrap();

    assert_eq!(
        reply_rx.recv().unwrap().unwrap(),
        ipc::PeerSendOutcome::Delivered
    );
    assert!(!app.peer_handovers.contains_key(&target));
    assert!(!app.peer_handover_consumed_tombstones.contains_key(&target));
    app.shutdown();
}

#[test]
fn duplicate_receipt_does_not_duplicate_handover_tracking() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Codex);
    let (reply_tx, _reply_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "once".into(), reply_tx);
    let delivery_id = peer_delivery_id(&rx);
    app.handle_peer_inbox_ack(target, delivery_id).unwrap();
    app.handle_peer_inbox_ack(target, delivery_id).unwrap();
    assert_eq!(app.peer_handovers[&target].len(), 1);
    app.shutdown();
}

#[test]
fn push_peer_receipt_is_not_tracked_as_a_handover() {
    let (mut app, sender, target, rx) = app_with_ready_peer(PeerClientKind::Claude);
    let (reply_tx, _reply_rx) = oneshot::channel();
    app.begin_peer_send(sender, &ipc::PaneRef::Id(target), "push".into(), reply_tx);
    let delivery_id = peer_delivery_id(&rx);
    app.handle_peer_inbox_ack(target, delivery_id).unwrap();
    assert!(!app.peer_handovers.contains_key(&target));
    app.shutdown();
}

#[test]
fn handover_cap_evicts_the_oldest_entry() {
    let (mut app, _sender, target, _rx) = app_with_ready_peer(PeerClientKind::Codex);
    let message = PendingPeerInboxMessage {
        from_pane: app.ws().focused_pane_id,
        from_name: None,
        from_kind: None,
        body: "x".into(),
        ts_ms: 0,
        debug_peer_inbox_sequence: None,
        requeued_delivery_id: None,
        requeued_nudge: None,
        system_generated: false,
    };
    for delivery_id in 1..=129 {
        app.track_peer_handover(target, delivery_id, &message);
    }
    let queue = &app.peer_handovers[&target];
    assert_eq!(queue.len(), PENDING_PEER_INBOX_MAX_MESSAGES);
    assert_eq!(queue.front().unwrap().delivery_id, 2);
    app.shutdown();
}

#[test]
fn codex_sender_loss_notice_uses_the_normal_nudge_path() {
    let (mut app, sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.peer_client_kinds.insert(sender, PeerClientKind::Codex);
    app.lose_peer_handovers(target, "subscriber_gone_timeout");
    let notice_delivery_id = rx
        .try_iter()
        .find_map(|event| match event {
            ipc::Event::PeerInbox {
                delivery_id: Some(id),
                target_pane,
                from_pane,
                ..
            } if target_pane == sender && from_pane == target => Some(id),
            _ => None,
        })
        .expect("loss notice delivery");
    assert_ne!(notice_delivery_id, delivery_id);
    app.handle_peer_inbox_ack(sender, notice_delivery_id)
        .unwrap();
    assert!(app.pending_codex_peer_messages.contains_key(&sender));
    assert!(!app.peer_handovers.contains_key(&sender));
    app.shutdown();
}

#[test]
fn closing_loss_notice_recipient_does_not_report_the_notice_as_lost() {
    let (mut app, sender, lost_target, _delivery_id, rx) = retain_one_codex_handover();
    app.handle_peer_set_ready(sender, PeerClientKind::Codex, true)
        .unwrap();
    app.lose_peer_handovers(lost_target, "subscriber_gone_timeout");
    let notice_delivery_id = rx
        .try_iter()
        .find_map(|event| match event {
            ipc::Event::PeerInbox {
                delivery_id: Some(id),
                target_pane,
                from_pane,
                ..
            } if target_pane == sender && from_pane == lost_target => Some(id),
            _ => None,
        })
        .expect("loss notice delivery");
    assert!(
        app.pending_peer_deliveries[&notice_delivery_id]
            .message
            .system_generated
    );

    app.handle_close(&ipc::PaneRef::Id(sender)).unwrap();

    assert!(rx
        .try_iter()
        .all(|event| !matches!(event, ipc::Event::PeerMessageLost { .. })));
    app.shutdown();
}

#[test]
fn missing_sender_drops_loss_notice_but_still_emits_loss_event() {
    let (mut app, _sender, target, delivery_id, rx) = retain_one_codex_handover();
    app.peer_handovers.get_mut(&target).unwrap()[0].from_pane = usize::MAX;
    app.lose_peer_handovers(target, "subscriber_gone_timeout");
    let events: Vec<_> = rx.try_iter().collect();
    assert!(events.iter().any(|event| matches!(
        event,
        ipc::Event::PeerMessageLost { delivery_id: id, .. } if *id == delivery_id
    )));
    assert!(events
        .iter()
        .all(|event| !matches!(event, ipc::Event::PeerInbox { .. })));
    app.shutdown();
}

#[test]
fn loss_notice_labels_the_original_timestamp_as_utc() {
    let (mut app, _sender, target, _delivery_id, rx) = retain_one_codex_handover();
    app.peer_handovers.get_mut(&target).unwrap()[0].ts_ms = 1_725_000_123_456;
    app.lose_peer_handovers(target, "subscriber_gone_timeout");

    let body = rx
        .try_iter()
        .find_map(|event| match event {
            ipc::Event::PeerInbox { body, .. } => Some(body),
            _ => None,
        })
        .expect("loss notice");
    assert!(body.contains("Sent at 1725000123.456000000 UTC"));
    app.shutdown();
}

fn setup_slow_codex_submit() -> (App, usize, String) {
    let mut app = App::new(40, 160).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    seed_codex_live_ready_placeholder(&mut app, pane_id);
    let message = PendingCodexPeerMessage {
        from_pane: 999,
        from_name: None,
        from_kind: Some(PeerClientKind::Claude),
    };
    let expected: String = format_codex_peer_message(&message)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    app.pending_codex_peer_messages.insert(
        pane_id,
        VecDeque::from([PendingCodexPeerDelivery::SubmitAt {
            message,
            retries_remaining: CODEX_PEER_NUDGE_MAX_RETRIES,
            created_at: Instant::now(),
            observed_prefix_len: 0,
            ready_at: Instant::now(),
            expires_at: Instant::now() + CODEX_PEER_NUDGE_COMMIT_TIMEOUT,
            expected_composer: expected.clone(),
            expected_composer_raw: None,
            delivery_sequence: None,
        }]),
    );
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();
    (app, pane_id, expected)
}

fn elapse_codex_submit(app: &mut App, pane_id: usize, elapsed: Duration) {
    match app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .unwrap()
        .front_mut()
        .unwrap()
    {
        PendingCodexPeerDelivery::SubmitAt {
            created_at,
            ready_at,
            expires_at,
            ..
        } => {
            *created_at -= elapsed;
            *ready_at -= elapsed;
            *expires_at -= elapsed;
        }
        other => panic!("expected submit stage, got {other:?}"),
    }
}

#[test]
fn submit_commit_tracks_slow_render_progress_beyond_original_timeout() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    for (len, elapsed_ms) in [(1, 1000), (44, 2000), (143, 2500)] {
        elapse_codex_submit(&mut app, pane_id, Duration::from_millis(elapsed_ms));
        seed_codex_idle_composer(&mut app, pane_id, &expected[..len]);
        app.flush_pending_codex_peer_messages();
        assert!(app.ws().panes[&pane_id].test_input().is_empty());
        assert!(matches!(app.pending_codex_peer_messages[&pane_id].front(),
            Some(PendingCodexPeerDelivery::SubmitAt { observed_prefix_len, expires_at, .. })
            if *observed_prefix_len == len && *expires_at > Instant::now()));
    }
    elapse_codex_submit(&mut app, pane_id, Duration::from_secs(1));
    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn submit_commit_render_progress_cannot_extend_absolute_cap() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    for len in 1..=60 {
        elapse_codex_submit(&mut app, pane_id, Duration::from_secs(1));
        seed_codex_idle_composer(&mut app, pane_id, &expected[..len]);
        app.flush_pending_codex_peer_messages();
        if len < 60 {
            assert!(app.ws().panes[&pane_id].test_input().is_empty());
            assert!(matches!(
                app.pending_codex_peer_messages[&pane_id].front(),
                Some(PendingCodexPeerDelivery::SubmitAt { .. })
            ));
        }
    }
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\x15");
    assert!(matches!(app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { retries_remaining, .. })
        if *retries_remaining == CODEX_PEER_NUDGE_MAX_RETRIES - 1));
    app.shutdown();
}

fn assert_submit_requeued(app: &App, pane_id: usize) {
    assert_eq!(app.pending_peer_message_count(pane_id), 1);
    assert!(matches!(app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { message, retries_remaining, .. })
        if message.from_pane == 999
            && *retries_remaining == CODEX_PEER_NUDGE_MAX_RETRIES - 1));
}

fn assert_changed_submit_preserved(make_text: impl FnOnce(&str) -> String, expired: bool) {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    let changed = make_text(&expected);
    seed_codex_idle_composer(&mut app, pane_id, &changed);
    {
        let parser = app.ws().panes[&pane_id].parser.lock().unwrap();
        assert_eq!(
            normalized_codex_composer_text(parser.screen()),
            Some(changed.clone())
        );
    }
    if expired {
        elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    }
    app.flush_pending_codex_peer_messages();
    assert!(
        app.ws().panes[&pane_id].test_input().is_empty(),
        "user text must neither submit nor clear"
    );
    assert_submit_requeued(&app, pane_id);
    // A retained user draft eventually surfaces through the existing notification path.
    if let Some(PendingCodexPeerDelivery::Draft { stalled_since, .. }) = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .unwrap()
        .front_mut()
    {
        *stalled_since -= CODEX_PEER_DRAFT_STALL_TIMEOUT;
    }
    app.flush_pending_codex_peer_messages();
    assert!(app.codex_peer_notification.is_some());
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    app.shutdown();
}

#[test]
fn submit_commit_partial_nudge_then_user_text_fails_fast_without_clear() {
    assert_changed_submit_preserved(
        |expected| format!("{}日本語の下書き", &expected[..143]),
        false,
    );
}

#[test]
fn submit_commit_user_draft_then_full_nudge_never_clears() {
    assert_changed_submit_preserved(|expected| format!("日本語の下書き{expected}"), true);
}

#[test]
fn submit_commit_full_nudge_then_user_text_never_clears_after_expiry() {
    assert_changed_submit_preserved(|expected| format!("{expected}ABC"), true);
}

#[test]
fn submit_commit_empty_scrape_waits_then_requeues_without_clear() {
    let (mut app, pane_id, _) = setup_slow_codex_submit();
    seed_pane_screen(&mut app, pane_id, b"\x1b[2J\x1b[H\xE2\x80\xBA \x1b[1;3H");
    {
        let parser = app.ws().panes[&pane_id].parser.lock().unwrap();
        assert_eq!(
            normalized_codex_composer_text(parser.screen()),
            Some(String::new())
        );
    }
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert_submit_requeued(&app, pane_id);
    app.shutdown();
}

#[test]
fn submit_commit_missing_scrape_waits_then_requeues_without_clear() {
    let (mut app, pane_id, _) = setup_slow_codex_submit();
    seed_pane_screen(&mut app, pane_id, b"\x1b[2J\x1b[H");
    {
        let parser = app.ws().panes[&pane_id].parser.lock().unwrap();
        assert_eq!(normalized_codex_composer_text(parser.screen()), None);
    }
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert_submit_requeued(&app, pane_id);
    app.shutdown();
}

#[test]
fn submit_commit_initial_placeholder_waits_without_render_evidence() {
    let (mut app, pane_id, _) = setup_slow_codex_submit();
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt {
            observed_prefix_len: 0,
            ..
        })
    ));
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert_eq!(app.pending_peer_message_count(pane_id), 1);
    app.shutdown();
}

#[test]
fn submit_commit_released_composer_drops_without_retry() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    seed_codex_idle_composer(&mut app, pane_id, &expected[..44]);
    app.flush_pending_codex_peer_messages();
    seed_codex_busy_placeholder(&mut app, pane_id);
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.pending_peer_message_count(pane_id), 0);
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    assert!(app.codex_peer_notification.is_none());
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    app.shutdown();
}

#[test]
fn submit_commit_stalled_prefix_clears_and_requeues_when_end_visible() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    seed_codex_idle_composer(&mut app, pane_id, &expected[..44]);
    app.flush_pending_codex_peer_messages();
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\x15");
    assert_submit_requeued(&app, pane_id);
    app.shutdown();
}

#[test]
fn submit_commit_stalled_prefix_with_hidden_end_never_clears() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    let (rows, _) = app.ws().panes[&pane_id]
        .parser
        .lock()
        .unwrap()
        .screen()
        .size();
    let screen = format!(
        "\x1b[2J\x1b[{};1H\u{203a} {}\x1b[{rows};1H  {}\x1b[{rows};10H",
        rows - 1,
        &expected[..20],
        &expected[20..44]
    );
    seed_pane_screen(&mut app, pane_id, screen.as_bytes());
    {
        let parser = app.ws().panes[&pane_id].parser.lock().unwrap();
        assert_eq!(
            normalized_codex_composer_text(parser.screen()),
            Some(expected[..44].to_string())
        );
    }
    app.flush_pending_codex_peer_messages();
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert_submit_requeued(&app, pane_id);
    app.shutdown();
}

#[test]
fn submit_commit_exhausted_retries_waits_for_focus() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    if let Some(PendingCodexPeerDelivery::SubmitAt {
        retries_remaining, ..
    }) = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .unwrap()
        .front_mut()
    {
        *retries_remaining = 0;
    }
    seed_codex_idle_composer(&mut app, pane_id, &expected[..44]);
    app.flush_pending_codex_peer_messages();
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert!(
        matches!(app.pending_codex_peer_messages[&pane_id].front(), Some(PendingCodexPeerDelivery::AwaitFocus { message, retries_remaining: 0, .. }) if message.from_pane == 999)
    );
    assert_eq!(app.pending_peer_message_count(pane_id), 1);
    app.shutdown();
}

#[test]
fn submit_commit_exact_composer_submits_even_after_deadline() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    seed_codex_idle_composer(&mut app, pane_id, &expected);
    elapse_codex_submit(&mut app, pane_id, Duration::from_secs(61));
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

fn assert_submit_observes_during_delay(complete: bool) {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    let rendered = if complete {
        expected.as_str()
    } else {
        &expected[..44]
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    if let Some(PendingCodexPeerDelivery::SubmitAt {
        ready_at,
        expires_at,
        ..
    }) = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .unwrap()
        .front_mut()
    {
        *ready_at = Instant::now() + Duration::from_secs(1);
        *expires_at = deadline;
    }
    seed_codex_idle_composer(&mut app, pane_id, rendered);
    app.flush_pending_codex_peer_messages();
    assert!(
        app.ws().panes[&pane_id].test_input().is_empty(),
        "observation during the delay must never send Enter"
    );
    assert!(
        matches!(app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt { observed_prefix_len, expires_at, .. })
        if *observed_prefix_len == rendered.len() && *expires_at == deadline),
        "record partial and complete rendering during the delay without extending expiry"
    );
    seed_codex_busy_placeholder(&mut app, pane_id);
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_peer_message_count(pane_id),
        0,
        "already submitted text must not be retried"
    );
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(app.codex_peer_notification.is_none());
    app.shutdown();
}

#[test]
fn submit_commit_observes_partial_render_during_delay_before_release() {
    assert_submit_observes_during_delay(false);
}

#[test]
fn submit_commit_observes_complete_render_during_delay_before_release() {
    assert_submit_observes_during_delay(true);
}

fn codex_nudge_substring(expected: &str) -> String {
    let user_text = "MCPtoolcheck_messages";
    assert!(expected.contains(user_text));
    assert!(!expected.starts_with(user_text));
    user_text.to_string()
}

#[test]
fn submit_commit_substring_user_draft_never_clears_after_expiry() {
    assert_changed_submit_preserved(codex_nudge_substring, true);
}

#[test]
fn submit_commit_substring_user_draft_never_counts_as_render_progress() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    seed_codex_idle_composer(&mut app, pane_id, &codex_nudge_substring(&expected));
    app.flush_pending_codex_peer_messages();
    assert_submit_requeued(&app, pane_id);
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    // Releasing this user draft must not erase the retained peer message.
    seed_codex_busy_placeholder(&mut app, pane_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.pending_peer_message_count(pane_id), 1);
    app.shutdown();
}

#[test]
fn submit_commit_substring_during_delay_does_not_arm_release() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    if let Some(PendingCodexPeerDelivery::SubmitAt { ready_at, .. }) = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .unwrap()
        .front_mut()
    {
        *ready_at += Duration::from_secs(1);
    }
    seed_codex_idle_composer(&mut app, pane_id, &codex_nudge_substring(&expected));
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt {
            observed_prefix_len: 0,
            ..
        })
    ));
    seed_codex_busy_placeholder(&mut app, pane_id);
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert_submit_requeued(&app, pane_id);
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    app.shutdown();
}

#[test]
fn submit_commit_visible_end_empty_scrape_is_preserved_with_prefix_clear_control() {
    for is_prefix in [false, true] {
        let (mut app, pane_id, expected) = setup_slow_codex_submit();
        // Both buffers use exactly the same prompt, separator, footer and cursor
        // geometry. The prefix case proves this layout permits recovery clearing.
        let text = if is_prefix { &expected[..10] } else { "" };
        let screen = format!(
            "\x1b[2J\x1b[H\u{203a} {text}\x1b[3;1H  gpt-5.6-sol medium \u{b7} cwd\x1b[3;3H"
        );
        seed_pane_screen(&mut app, pane_id, screen.as_bytes());
        {
            let parser = app.ws().panes[&pane_id].parser.lock().unwrap();
            assert_eq!(
                normalized_codex_composer_text(parser.screen()),
                Some(text.to_string())
            );
        }
        app.flush_pending_codex_peer_messages();
        assert!(app.ws().panes[&pane_id].test_input().is_empty());
        elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
        app.flush_pending_codex_peer_messages();
        assert_submit_requeued(&app, pane_id);
        let expected_input: &[u8] = if is_prefix { b"\x15" } else { b"" };
        assert_eq!(
            app.ws().panes[&pane_id].test_input(),
            expected_input,
            "visible-end empty text must be preserved, while our real prefix must clear"
        );
        app.shutdown();
    }
}

#[test]
fn submit_commit_slow_render_survives_null_scrape_between_progress_ticks() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    if let Some(PendingCodexPeerDelivery::SubmitAt { ready_at, .. }) = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .unwrap()
        .front_mut()
    {
        *ready_at += Duration::from_secs(1);
    }
    let mut observed = 0;
    // Replay the 00:17 field shape: early paint during the delay, a transient
    // missing scrape, then continued growth beyond the original five seconds.
    for (elapsed_ms, len) in [
        (100, Some(1)),
        (200, Some(1)),
        (200, Some(6)),
        (600, Some(6)),
        (300, Some(9)),
        (1000, Some(35)),
        (300, Some(35)),
        (300, Some(36)),
        (300, Some(41)),
        (300, Some(41)),
        (300, Some(42)),
        (500, Some(43)),
        (520, None),
        (212, Some(48)),
    ] {
        elapse_codex_submit(&mut app, pane_id, Duration::from_millis(elapsed_ms));
        if let Some(len) = len {
            seed_codex_idle_composer(&mut app, pane_id, &expected[..len]);
            observed = observed.max(len);
        } else {
            seed_pane_screen(&mut app, pane_id, b"\x1b[2J\x1b[H");
        }
        app.flush_pending_codex_peer_messages();
        assert!(app.ws().panes[&pane_id].test_input().is_empty());
        assert!(
            matches!(app.pending_codex_peer_messages[&pane_id].front(),
            Some(PendingCodexPeerDelivery::SubmitAt { observed_prefix_len, .. })
            if *observed_prefix_len == observed),
            "a null scrape must preserve progress and keep waiting"
        );
    }
    elapse_codex_submit(&mut app, pane_id, Duration::from_secs(1));
    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert_eq!(app.pending_peer_message_count(pane_id), 0);
    app.shutdown();
}

fn stale_test_message(from_pane: usize, from_name: Option<&str>) -> PendingCodexPeerMessage {
    PendingCodexPeerMessage {
        from_pane,
        from_name: from_name.map(str::to_string),
        from_kind: Some(PeerClientKind::Claude),
    }
}

fn normalized_stale_test_message(message: &PendingCodexPeerMessage) -> String {
    format_codex_peer_message(message)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

fn queue_stale_test_delivery(
    app: &mut App,
    pane_id: usize,
    delivery: PendingCodexPeerDelivery,
    injected: &[String],
) {
    app.pending_codex_peer_messages
        .insert(pane_id, VecDeque::from([delivery]));
    app.codex_peer_injected_composers
        .insert(pane_id, injected.iter().cloned().collect());
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();
}

fn stale_test_draft(message: PendingCodexPeerMessage) -> PendingCodexPeerDelivery {
    PendingCodexPeerDelivery::Draft {
        message,
        retries_remaining: CODEX_PEER_NUDGE_MAX_RETRIES,
        stalled_since: Instant::now(),
        delivery_sequence: Some(700),
    }
}

fn stale_test_await_focus(message: PendingCodexPeerMessage) -> PendingCodexPeerDelivery {
    PendingCodexPeerDelivery::AwaitFocus {
        message,
        retries_remaining: 0,
        delivery_sequence: Some(701),
    }
}

#[test]
fn released_leftover_is_submitted_by_the_next_draft() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    app.codex_peer_injected_composers
        .insert(pane_id, VecDeque::from([expected.clone()]));
    seed_codex_idle_composer(&mut app, pane_id, &expected[..44]);
    app.flush_pending_codex_peer_messages();
    seed_codex_busy_placeholder(&mut app, pane_id);
    elapse_codex_submit(&mut app, pane_id, CODEX_PEER_NUDGE_COMMIT_TIMEOUT);
    app.flush_pending_codex_peer_messages();
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    let focused_id = app
        .handle_split(
            &PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split focus away from stale pane");
    assert_eq!(app.ws().focused_pane_id, focused_id);

    let next = stale_test_message(999, None);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(next),
        std::slice::from_ref(&expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    assert!(!app.codex_peer_injected_composers.contains_key(&pane_id));
    assert!(app.codex_peer_notification.is_none());
    app.shutdown();
}

#[test]
fn draft_submits_two_concatenated_own_injections_once() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(11, Some("worker"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        std::slice::from_ref(&expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &format!("{expected}{expected}"));

    app.flush_pending_codex_peer_messages();
    for _ in 0..3 {
        app.flush_pending_codex_peer_messages();
    }

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn await_focus_submits_two_concatenated_own_injections_once() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(11, Some("worker"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_await_focus(message),
        std::slice::from_ref(&expected),
    );
    seed_pane_screen(&mut app, pane_id, b"\x1b[2J\x1b[H");
    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));
    seed_codex_idle_composer(&mut app, pane_id, &format!("{expected}{expected}"));

    app.flush_pending_codex_peer_messages();
    for _ in 0..3 {
        app.flush_pending_codex_peer_messages();
    }

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn different_sender_leftover_submits_before_current_draft_writes_once() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let old_message = stale_test_message(14, Some("old-worker"));
    let current_message = stale_test_message(23, Some("current-worker"));
    let old_expected = normalized_stale_test_message(&old_message);
    let current_payload = format_codex_peer_message(&current_message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(current_message),
        std::slice::from_ref(&old_expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &old_expected);

    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();
    seed_codex_live_ready_placeholder(&mut app, pane_id);
    app.flush_pending_codex_peer_messages();

    assert_eq!(
        app.ws().panes[&pane_id].test_input(),
        current_payload.as_bytes()
    );
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn draft_submits_mixed_recorded_injections_when_current_text_is_present() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let old_message = stale_test_message(14, Some("old-worker"));
    let current_message = stale_test_message(23, Some("current-worker"));
    let old_expected = normalized_stale_test_message(&old_message);
    let current_expected = normalized_stale_test_message(&current_message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(current_message),
        &[old_expected.clone(), current_expected.clone()],
    );
    seed_codex_idle_composer(
        &mut app,
        pane_id,
        &format!("{current_expected}{old_expected}{current_expected}"),
    );

    app.flush_pending_codex_peer_messages();

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn differing_user_draft_is_never_submitted_and_still_reaches_await_focus() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(7, Some("advisor"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        std::slice::from_ref(&expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &format!("{expected}usertext"));
    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    if let Some(PendingCodexPeerDelivery::Draft { stalled_since, .. }) = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .unwrap()
        .front_mut()
    {
        *stalled_since -= CODEX_PEER_DRAFT_STALL_TIMEOUT;
    }
    seed_pane_screen(&mut app, pane_id, b"\x1b[2J\x1b[H");
    app.flush_pending_codex_peer_messages();

    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));
    assert!(app.codex_peer_injected_composers.contains_key(&pane_id));
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    app.shutdown();
}

#[test]
fn idle_bare_prompt_with_stale_text_never_enters_await_focus() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(7, None);
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        std::slice::from_ref(&expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &expected);

    app.flush_pending_codex_peer_messages();

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    assert!(app.codex_peer_notification.is_none());
    app.shutdown();
}

#[test]
fn await_focus_stale_recognizer_requires_visible_composer_end() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(31, Some("recorded"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_await_focus(message),
        std::slice::from_ref(&expected),
    );
    let (rows, _) = app.ws().panes[&pane_id]
        .parser
        .lock()
        .unwrap()
        .screen()
        .size();
    let screen = format!(
        "\x1b[2J\x1b[{};1H\u{203a} {}\x1b[{rows};1H  {}\x1b[{rows};10H",
        rows - 1,
        &expected[..20],
        &expected[20..]
    );
    seed_pane_screen(&mut app, pane_id, screen.as_bytes());

    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));

    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    app.shutdown();
}

#[test]
fn await_focus_stale_injection_waits_for_idle_before_enter() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(32, Some("busy-await"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_await_focus(message),
        std::slice::from_ref(&expected),
    );
    seed_codex_busy_composer(&mut app, pane_id, &expected);

    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::AwaitFocus { .. })
    ));

    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    app.shutdown();
}

#[test]
fn draft_stale_injection_waits_for_idle_before_enter() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(33, Some("busy-draft"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        std::slice::from_ref(&expected),
    );
    seed_codex_busy_composer(&mut app, pane_id, &expected);

    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));

    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    app.shutdown();
}

#[test]
fn focusing_draft_stale_injection_uses_notification_without_enter() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(34, Some("focus-draft"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        std::slice::from_ref(&expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &expected);

    app.handle_focus(&PaneRef::Id(pane_id))
        .expect("focus stale Draft pane");

    assert!(app.visible_codex_peer_notification().is_some());
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn focusing_await_focus_stale_injection_uses_notification_without_enter() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(35, Some("focus-await"));
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_await_focus(message),
        std::slice::from_ref(&expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &expected);

    app.handle_focus(&PaneRef::Id(pane_id))
        .expect("focus stale AwaitFocus pane");

    assert!(app.visible_codex_peer_notification().is_some());
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn await_focus_stale_enter_clears_history_before_lagging_frame() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let first = stale_test_message(36, Some("first"));
    let first_expected = normalized_stale_test_message(&first);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_await_focus(first),
        std::slice::from_ref(&first_expected),
    );
    seed_codex_idle_composer(&mut app, pane_id, &first_expected);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();

    let next = stale_test_message(37, Some("next"));
    let next_payload = format_codex_peer_message(&next);
    app.pending_codex_peer_messages
        .insert(pane_id, VecDeque::from([stale_test_draft(next)]));
    seed_codex_idle_composer(&mut app, pane_id, &first_expected);
    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));

    seed_codex_live_ready_placeholder(&mut app, pane_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.ws().panes[&pane_id].test_input(),
        next_payload.as_bytes()
    );
    app.shutdown();
}

#[test]
fn submit_at_clear_forgets_injection_before_tail_repaint() {
    let (mut app, sender_id, pane_id) = setup_unfocused_registered_codex();
    seed_codex_live_ready_placeholder(&mut app, pane_id);
    app.handle_peer_send(
        sender_id,
        &PaneRef::Id(pane_id),
        "clear history".to_string(),
    )
    .expect("queue nudge");
    app.flush_pending_codex_peer_messages();
    let (message, expected) = match app.pending_codex_peer_messages[&pane_id]
        .front()
        .expect("SubmitAt")
    {
        PendingCodexPeerDelivery::SubmitAt {
            message,
            expected_composer,
            ..
        } => (message.clone(), expected_composer.clone()),
        other => panic!("expected SubmitAt, got {other:?}"),
    };
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();
    seed_codex_idle_composer(&mut app, pane_id, &expected[..44]);
    app.flush_pending_codex_peer_messages();
    if let Some(PendingCodexPeerDelivery::SubmitAt {
        created_at,
        ready_at,
        expires_at,
        ..
    }) = app
        .pending_codex_peer_messages
        .get_mut(&pane_id)
        .and_then(|queue| queue.front_mut())
    {
        let elapsed = CODEX_PEER_NUDGE_COMMIT_TIMEOUT + Duration::from_millis(1);
        *created_at -= elapsed;
        *ready_at -= elapsed;
        *expires_at -= elapsed;
    }
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\x15");
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();

    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::Draft { .. })
    ));

    let payload = format_codex_peer_message(&message);
    seed_codex_live_ready_placeholder(&mut app, pane_id);
    app.flush_pending_codex_peer_messages();
    assert_eq!(app.ws().panes[&pane_id].test_input(), payload.as_bytes());
    app.shutdown();
}

#[test]
fn focused_route_history_recognizes_leftover_after_focus_leaves() {
    let (mut app, sender_id, pane_id) = setup_unfocused_registered_codex();
    seed_codex_live_ready_placeholder(&mut app, pane_id);
    app.handle_focus(&PaneRef::Id(pane_id))
        .expect("focus Codex target");
    app.handle_peer_send(
        sender_id,
        &PaneRef::Id(pane_id),
        "focused route".to_string(),
    )
    .expect("send through focused route");
    let (message, expected) = match app.pending_codex_peer_messages[&pane_id]
        .front()
        .expect("SubmitAt")
    {
        PendingCodexPeerDelivery::SubmitAt {
            message,
            expected_composer,
            ..
        } => (message.clone(), expected_composer.clone()),
        other => panic!("expected SubmitAt, got {other:?}"),
    };
    app.pending_codex_peer_messages
        .insert(pane_id, VecDeque::from([stale_test_draft(message)]));
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();
    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.handle_focus(&PaneRef::Id(sender_id))
        .expect("leave Codex target");

    app.flush_pending_codex_peer_messages();

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn real_writes_keep_two_distinct_injections_for_concatenation() {
    let (mut app, sender_id, pane_id) = setup_unfocused_registered_codex();
    seed_codex_live_ready_placeholder(&mut app, pane_id);
    app.handle_focus(&PaneRef::Id(pane_id))
        .expect("focus first route");
    app.handle_peer_send(sender_id, &PaneRef::Id(pane_id), "first".to_string())
        .expect("write first nudge");
    let (first_message, first_expected) = match app.pending_codex_peer_messages[&pane_id]
        .front()
        .expect("first SubmitAt")
    {
        PendingCodexPeerDelivery::SubmitAt {
            message,
            expected_composer,
            ..
        } => (message.clone(), expected_composer.clone()),
        other => panic!("expected first SubmitAt, got {other:?}"),
    };
    app.pending_codex_peer_messages
        .insert(pane_id, VecDeque::from([stale_test_draft(first_message)]));
    seed_codex_idle_composer(&mut app, pane_id, &first_expected);
    app.flush_pending_codex_peer_messages();
    assert!(app.visible_codex_peer_notification().is_some());
    app.dismiss_codex_peer_notification();
    app.handle_focus(&PaneRef::Id(sender_id))
        .expect("leave Codex target");

    seed_codex_live_ready_placeholder(&mut app, pane_id);
    app.handle_peer_send(pane_id, &PaneRef::Id(pane_id), "second".to_string())
        .expect("queue self-send");
    app.flush_pending_codex_peer_messages();
    let (second_message, second_expected) = match app.pending_codex_peer_messages[&pane_id]
        .front()
        .expect("second SubmitAt")
    {
        PendingCodexPeerDelivery::SubmitAt {
            message,
            expected_composer,
            ..
        } => (message.clone(), expected_composer.clone()),
        other => panic!("expected second SubmitAt, got {other:?}"),
    };
    assert_ne!(first_expected, second_expected);
    app.pending_codex_peer_messages
        .insert(pane_id, VecDeque::from([stale_test_draft(second_message)]));
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .unwrap()
        .clear_test_input();
    seed_codex_idle_composer(
        &mut app,
        pane_id,
        &format!("{first_expected}{second_expected}"),
    );

    app.flush_pending_codex_peer_messages();

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn empty_end_visible_composer_is_not_a_stale_injection() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(38, Some("empty"));
    let payload = format_codex_peer_message(&message);
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        std::slice::from_ref(&expected),
    );
    seed_pane_screen(
        &mut app,
        pane_id,
        b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA \r\n\r\n  gpt-5.6-sol medium \xC2\xB7 cwd\x1b[1;3H",
    );

    app.flush_pending_codex_peer_messages();

    assert_eq!(app.ws().panes[&pane_id].test_input(), payload.as_bytes());
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    app.shutdown();
}

#[test]
fn end_invisible_exact_text_never_triggers_stale_enter_or_clear() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(7, None);
    let expected = normalized_stale_test_message(&message);
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        std::slice::from_ref(&expected),
    );
    let (rows, _) = app.ws().panes[&pane_id]
        .parser
        .lock()
        .unwrap()
        .screen()
        .size();
    let screen = format!(
        "\x1b[2J\x1b[{};1H\u{203a} {}\x1b[{rows};1H  {}\x1b[{rows};10H",
        rows - 1,
        &expected[..20],
        &expected[20..]
    );
    seed_pane_screen(&mut app, pane_id, screen.as_bytes());

    app.flush_pending_codex_peer_messages();

    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(app.codex_peer_injected_composers.contains_key(&pane_id));
    assert!(app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn end_invisible_other_sender_row_does_not_diverge_submit_at() {
    let (mut app, pane_id, expected) = setup_slow_codex_submit();
    app.codex_peer_injected_composers
        .insert(pane_id, VecDeque::from([expected.clone()]));
    let other = normalized_stale_test_message(&stale_test_message(14, Some("other")));
    let (rows, _) = app.ws().panes[&pane_id]
        .parser
        .lock()
        .unwrap()
        .screen()
        .size();
    let screen = format!(
        "\x1b[2J\x1b[{};1H\u{203a} {}\x1b[{rows};1H  {}\x1b[{rows};10H",
        rows - 1,
        &other[..20],
        &other[20..44]
    );
    seed_pane_screen(&mut app, pane_id, screen.as_bytes());

    app.flush_pending_codex_peer_messages();
    assert!(matches!(
        app.pending_codex_peer_messages[&pane_id].front(),
        Some(PendingCodexPeerDelivery::SubmitAt { .. })
    ));
    assert!(app.ws().panes[&pane_id].test_input().is_empty());
    assert!(app.codex_peer_injected_composers.contains_key(&pane_id));
    seed_codex_idle_composer(&mut app, pane_id, &expected);
    app.flush_pending_codex_peer_messages();

    assert_eq!(app.ws().panes[&pane_id].test_input(), b"\r");
    assert!(!app.pending_codex_peer_messages.contains_key(&pane_id));
    app.shutdown();
}

#[test]
fn stale_waiting_trace_is_deduplicated_over_repeated_busy_frames() {
    let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for stage in ["draft", "await_focus", "user_draft_control"] {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "renga-codex-peer-stale-wait-{stage}-{}-{unique}.jsonl",
            std::process::id()
        ));
        let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
        let message = stale_test_message(40, Some(stage));
        let expected = normalized_stale_test_message(&message);
        let delivery = if stage == "draft" {
            stale_test_draft(message)
        } else {
            stale_test_await_focus(message)
        };
        queue_stale_test_delivery(&mut app, pane_id, delivery, std::slice::from_ref(&expected));
        if stage == "user_draft_control" {
            seed_codex_busy_composer(&mut app, pane_id, "keep this user draft");
        } else {
            seed_codex_busy_composer(&mut app, pane_id, &expected);
        }
        set_codex_peer_debug_log_path_test_override(Some(Some(path.as_os_str().to_owned())));

        for _ in 0..50 {
            app.flush_pending_codex_peer_messages();
        }
        set_codex_peer_debug_log_path_test_override(Some(None));
        app.shutdown();

        let records: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .expect("busy wait trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace record"))
            .collect();
        let expected_action = if stage == "user_draft_control" {
            "await_focus_waiting"
        } else {
            "stale_injection_waiting_until_idle"
        };
        assert_eq!(records.len(), 1, "{stage} emitted {records:?}");
        assert_eq!(records[0]["action"], expected_action, "{stage}");
        if stage == "user_draft_control" {
            assert!(records[0].get("matched_injections").is_none());
        } else {
            assert_eq!(
                records[0]["matched_injections"],
                serde_json::json!([expected])
            );
        }
        std::fs::remove_file(path).expect("remove busy wait trace");
    }
}

struct CodexPeerLogEnvRestore(Option<std::ffi::OsString>);

impl Drop for CodexPeerLogEnvRestore {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", value),
            None => std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG"),
        }
    }
}

#[test]
fn stale_trace_emitter_stops_after_trace_is_disabled() {
    let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let previous = std::env::var_os("RENGA_DEBUG_CODEX_PEER_LOG");
    let _restore = CodexPeerLogEnvRestore(previous);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "renga-codex-peer-stale-toggle-{}-{unique}.jsonl",
        std::process::id()
    ));
    std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", path.as_os_str());

    for mode in ["enabled", "forced_disabled", "env_unset"] {
        if mode == "env_unset" {
            std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG");
        }
        set_codex_peer_debug_log_path_test_override(match mode {
            "enabled" => None,
            "forced_disabled" => Some(None),
            "env_unset" => None,
            _ => unreachable!(),
        });
        let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
        let message = stale_test_message(41, Some(mode));
        let expected = normalized_stale_test_message(&message);
        queue_stale_test_delivery(
            &mut app,
            pane_id,
            stale_test_draft(message),
            std::slice::from_ref(&expected),
        );
        seed_codex_idle_composer(&mut app, pane_id, &expected);
        app.flush_pending_codex_peer_messages();
        set_codex_peer_debug_log_path_test_override(Some(None));
        app.shutdown();
    }

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .expect("enabled stale trace")
        .lines()
        .map(|line| serde_json::from_str(line).expect("trace record"))
        .collect();
    assert_eq!(records.len(), 1, "disabled paths leaked: {records:?}");
    assert_eq!(records[0]["action"], "stale_injection_submitted");
    std::fs::remove_file(path).expect("remove stale toggle trace");
}

#[test]
fn stale_submission_trace_includes_expected_and_screen_composer() {
    let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "renga-codex-peer-stale-{}-{unique}.jsonl",
        std::process::id()
    ));
    set_codex_peer_debug_log_path_test_override(Some(Some(path.as_os_str().to_owned())));
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    let message = stale_test_message(7, Some("advisor"));
    let old_message = stale_test_message(8, Some("older"));
    let unused_message = stale_test_message(9, Some("unused"));
    let expected = normalized_stale_test_message(&message);
    let old_expected = normalized_stale_test_message(&old_message);
    let unused_expected = normalized_stale_test_message(&unused_message);
    let screen_composer = format!("{expected}{old_expected}{expected}");
    queue_stale_test_delivery(
        &mut app,
        pane_id,
        stale_test_draft(message),
        &[expected.clone(), old_expected.clone(), unused_expected],
    );
    seed_codex_idle_composer(&mut app, pane_id, &screen_composer);

    app.flush_pending_codex_peer_messages();
    app.shutdown();
    set_codex_peer_debug_log_path_test_override(Some(None));

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .expect("stale trace JSONL")
        .lines()
        .map(|line| serde_json::from_str(line).expect("trace record"))
        .collect();
    let record = records
        .iter()
        .find(|record| record["action"] == "stale_injection_submitted")
        .expect("stale submission trace");
    assert_eq!(record["expected_composer"], screen_composer);
    assert_eq!(record["screen_composer"], screen_composer);
    assert_eq!(
        record["matched_injections"],
        serde_json::json!([expected, old_expected, expected])
    );
    assert_eq!(record["composer_matches"], true);
    assert_eq!(record["composer_end_visible"], true);
    std::fs::remove_file(path).expect("remove stale trace JSONL");
}

#[test]
fn subscriber_gone_forgets_injected_composer_text() {
    let (mut app, _sender_id, pane_id) = setup_unfocused_registered_codex();
    app.peer_live_subscribers.insert(pane_id);
    app.codex_peer_injected_composers
        .insert(pane_id, VecDeque::from(["oldnudge".to_string()]));

    app.handle_peer_subscriber_gone(pane_id, "test_disconnect");

    assert!(!app.codex_peer_injected_composers.contains_key(&pane_id));
    app.shutdown();
}
