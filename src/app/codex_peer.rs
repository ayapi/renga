use super::*;

pub(crate) const CODEX_APPEND_ENTER_DELAY: Duration = Duration::from_millis(75);
pub(crate) const CODEX_PEER_NUDGE_COMMIT_DELAY: Duration = Duration::from_millis(1000);
pub(crate) const CODEX_PEER_NUDGE_COMMIT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const CODEX_APPEND_ENTER_SNAPSHOT_LINES: usize = 8;

/// Window during which a `(target, from, body)` triple is treated as
/// a re-send and dropped before reaching `Event::PeerInbox`. Set to a
/// small handful of seconds so legitimate retries after the
/// receiver's reply still get through, but a dispatcher / worker
/// that fires the exact same payload twice in quick succession
/// can't double-paper the transcript with phantom user turns. See
/// renga#221 acceptance criterion #2.
pub(crate) const PEER_SEND_DEDUPE_TTL: Duration = Duration::from_secs(5);
// Keep a pre-registration burst below the EventBus subscriber capacity
// (256) and cap retained body storage. Refusing the newest send is
// deliberate: the sender receives an actionable error instead of a
// successful response for data that cannot be retained reliably.
pub(crate) const PENDING_PEER_INBOX_MAX_MESSAGES: usize = 128;
pub(crate) const PENDING_PEER_INBOX_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingPeerInboxMessage {
    pub(crate) from_pane: usize,
    pub(crate) from_name: Option<String>,
    pub(crate) from_kind: Option<PeerClientKind>,
    pub(crate) body: String,
    pub(crate) ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCodexPeerMessage {
    pub(crate) from_pane: usize,
    pub(crate) from_name: Option<String>,
    pub(crate) from_kind: Option<PeerClientKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPeerNotificationState {
    pub(crate) target_pane: usize,
    pub(crate) message: PendingCodexPeerMessage,
    pub(crate) pending_count: usize,
}

impl CodexPeerNotificationState {
    fn register_message(&mut self, message: PendingCodexPeerMessage) {
        self.message = message;
        self.pending_count = self.pending_count.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingCodexPeerDelivery {
    Draft(PendingCodexPeerMessage),
    SubmitAt(Instant),
    QueueAt {
        ready_at: Instant,
        expires_at: Instant,
        message: PendingCodexPeerMessage,
        expected_composer: String,
    },
}

#[derive(Debug)]
struct CodexPeerScreenSnapshot {
    has_draft: Option<bool>,
    composer: Option<String>,
    ready_for_nudge: bool,
    can_queue_message: bool,
    busy_queue_available: bool,
    can_submit_message: bool,
}

pub(crate) fn screen_tail_lines(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let (cursor_row, _) = screen.cursor_position();
    let mut last_content_row = None;
    for row in 0..rows {
        let mut has_text = false;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    has_text = true;
                    break;
                }
            }
        }
        if has_text {
            last_content_row = Some(row);
        }
    }
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    let mut lines =
        Vec::with_capacity(end_row.saturating_sub(start_row).saturating_add(1) as usize);
    for row in start_row..=end_row {
        let mut line = String::with_capacity(cols as usize);
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

pub(crate) fn screen_has_visible_text(screen: &vt100::Screen) -> bool {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn codex_prompt_allows_peer_nudge_on_screen(screen: &vt100::Screen) -> Option<bool> {
    if screen.hide_cursor() {
        return Some(false);
    }
    let (rows, cols) = screen.size();
    let mut last_content_row = None;
    for row in 0..rows {
        let mut has_text = false;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    has_text = true;
                    break;
                }
            }
        }
        if has_text {
            last_content_row = Some(row);
        }
    }
    let mut prompt_row = None;
    let (cursor_row, cursor_col) = screen.cursor_position();
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    for row in (start_row..=end_row).rev() {
        let mut line = String::with_capacity(cols as usize);
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        if line.trim_start().starts_with('›') {
            prompt_row = Some(row);
            break;
        }
    }
    let prompt_row = prompt_row?;
    if cursor_row > prompt_row && !cursor_is_on_codex_footer(screen, prompt_row, cursor_row, cols) {
        return Some(false);
    }
    if cursor_row == prompt_row && cursor_col > 2 {
        return Some(false);
    }
    Some(true)
}

fn looks_like_codex_placeholder(text: &str) -> bool {
    let normalized = text.trim();
    normalized.eq_ignore_ascii_case("Ask Codex anything...")
        || normalized.eq_ignore_ascii_case("Ask Codex anything")
}

fn screen_row_has_visible_text(screen: &vt100::Screen, row: u16, cols: u16) -> bool {
    (0..cols).any(|col| {
        screen
            .cell(row, col)
            .is_some_and(|cell| !cell.contents().trim().is_empty())
    })
}

fn cursor_is_on_codex_footer(
    screen: &vt100::Screen,
    prompt_row: u16,
    cursor_row: u16,
    cols: u16,
) -> bool {
    cursor_row > prompt_row.saturating_add(1)
        && screen_row_has_visible_text(screen, cursor_row, cols)
        && (prompt_row.saturating_add(1)..cursor_row)
            .all(|row| !screen_row_has_visible_text(screen, row, cols))
}

pub(crate) fn codex_composer_has_draft_on_screen(screen: &vt100::Screen) -> Option<bool> {
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let mut last_content_row = None;
    for row in 0..rows {
        let mut has_text = false;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    has_text = true;
                    break;
                }
            }
        }
        if has_text {
            last_content_row = Some(row);
        }
    }
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    for row in (start_row..=end_row).rev() {
        let mut prompt_col = None;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if cell.contents() == "›" {
                    prompt_col = Some(col);
                    break;
                }
            }
        }
        let Some(prompt_col) = prompt_col else {
            continue;
        };
        let input_start = prompt_col.saturating_add(1);
        let editable_start = if screen
            .cell(row, input_start)
            .is_some_and(|cell| cell.contents().trim().is_empty())
        {
            input_start.saturating_add(1)
        } else {
            input_start
        };
        if cursor_row > row && !cursor_is_on_codex_footer(screen, row, cursor_row, cols) {
            return Some(true);
        }
        if cursor_row == row {
            if cursor_col > editable_start {
                return Some(true);
            }
            return Some(false);
        }

        let mut input_text = String::new();
        let mut has_input_text = false;
        let mut has_normal_input_text = false;
        for col in input_start..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            input_text.push_str(cell.contents());
            if cell.contents().trim().is_empty() {
                continue;
            }
            has_input_text = true;
            if !cell.dim() {
                has_normal_input_text = true;
            }
        }
        if !has_input_text || looks_like_codex_placeholder(&input_text) {
            return Some(false);
        }
        return Some(has_normal_input_text);
    }
    None
}

pub(crate) fn normalized_codex_composer_text(screen: &vt100::Screen) -> Option<String> {
    let (rows, cols) = screen.size();
    let (cursor_row, _) = screen.cursor_position();
    let last_content_row = (0..rows)
        .rev()
        .find(|row| screen_row_has_visible_text(screen, *row, cols));
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    let (prompt_row, prompt_col) = (start_row..=end_row).rev().find_map(|row| {
        (0..cols).find_map(|col| {
            screen
                .cell(row, col)
                .is_some_and(|cell| cell.contents() == "\u{203a}")
                .then_some((row, col))
        })
    })?;
    let input_start = prompt_col.saturating_add(1);
    let editable_start = if screen
        .cell(prompt_row, input_start)
        .is_some_and(|cell| cell.contents().trim().is_empty())
    {
        input_start.saturating_add(1)
    } else {
        input_start
    };
    let mut text = String::new();
    for row in prompt_row..rows {
        let col_start = if row == prompt_row { editable_start } else { 0 };
        let mut line = String::new();
        for col in col_start..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        if row > prompt_row && line.trim().is_empty() {
            break;
        }
        text.push_str(&line);
    }
    Some(
        text.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>(),
    )
}

fn normalize_codex_composer_expected(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn normalized_screen_rows(screen: &vt100::Screen, start: u16, end: u16) -> String {
    let (_, cols) = screen.size();
    let mut text = String::new();
    for row in start..end {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                text.push_str(cell.contents());
            }
        }
    }
    text.chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn codex_peer_screen_snapshot(screen: &vt100::Screen) -> CodexPeerScreenSnapshot {
    let has_draft = codex_composer_has_draft_on_screen(screen);
    let composer = normalized_codex_composer_text(screen);
    let (rows, cols) = screen.size();
    let prompt_row = (0..rows).rev().find(|row| {
        (0..cols).any(|col| {
            screen
                .cell(*row, col)
                .is_some_and(|cell| cell.contents() == "\u{203a}")
        })
    });
    let (status, footer) = prompt_row.map_or_else(
        || (String::new(), String::new()),
        |prompt_row| {
            (
                normalized_screen_rows(screen, prompt_row.saturating_sub(4), prompt_row),
                normalized_screen_rows(
                    screen,
                    prompt_row.saturating_add(1),
                    prompt_row.saturating_add(8).min(rows),
                ),
            )
        },
    );
    // Positive detection controls whether renga may inject and press Tab, so
    // anchor the busy signal above the composer and the queue action below it.
    // Transcript mentions and unknown future UI safely remain pending.
    let busy = status.contains("working(") && status.contains("esctointerrupt");
    let busy_queue_available =
        !screen.hide_cursor() && busy && footer.contains("tabtoqueuemessage");
    let can_queue_message = !screen.hide_cursor() && has_draft == Some(false) && busy;
    let can_submit_message =
        !busy && (footer.contains("entertosend") || footer.contains("readyforinput"));
    let ready_for_nudge = !busy
        && screen_has_visible_text(screen)
        && (codex_prompt_allows_peer_nudge_on_screen(screen).unwrap_or(can_submit_message));
    CodexPeerScreenSnapshot {
        has_draft,
        composer,
        ready_for_nudge,
        can_queue_message,
        busy_queue_available,
        can_submit_message,
    }
}

fn codex_composer_has_draft(pane: &Pane) -> Option<bool> {
    let Ok(parser) = pane.parser.lock() else {
        return None;
    };
    codex_composer_has_draft_on_screen(parser.screen())
}

fn pending_startup_looks_like_codex(pane: &Pane) -> bool {
    pane.pending_startup
        .as_ref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|text| text.trim_start().starts_with("codex"))
}

pub(crate) fn format_codex_peer_message(msg: &PendingCodexPeerMessage) -> String {
    let mut header = format!("Peer request from id={}", msg.from_pane);
    if let Some(name) = &msg.from_name {
        header.push_str(&format!(" name={name}"));
    }
    if let Some(kind) = msg.from_kind {
        let kind = match kind {
            PeerClientKind::Claude => "claude",
            PeerClientKind::Codex => "codex",
        };
        header.push_str(&format!(" kind={kind}"));
    }
    let guidance = "Run check_messages now. Treat each returned message as a direct coworker request: do the requested work, and use send_message only when a reply or status update is needed.";
    format!("{header}. {guidance}")
}

pub(crate) fn write_input_to_pane(
    pane: &mut Pane,
    data: &[u8],
    append_enter: bool,
) -> std::result::Result<(), ipc::CodedError> {
    pane.write_input(data)
        .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
    if append_enter {
        if !data.is_empty() && (pane.is_codex_running() || pending_startup_looks_like_codex(pane)) {
            std::thread::sleep(CODEX_APPEND_ENTER_DELAY);
        }
        pane.write_input(b"\r")
            .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
    }
    Ok(())
}

impl App {
    /// Route `body` from `from_pane` to `target` when both share a
    /// workspace. Cross-tab targets are silently dropped so the MCP
    /// server cannot enumerate panes in other tabs by probing ids.
    /// Self-sends loop back to the sender pane: tooling like
    /// claude-org-ja's peer_notify resolves "secretary" from a shell
    /// running inside the secretary pane, and a silent drop there
    /// breaks the notification round-trip (see renga#215).
    pub(crate) fn handle_peer_send(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
    ) -> std::result::Result<ipc::PeerSendOutcome, ipc::CodedError> {
        let (sender_ws, _) = self
            .resolve_pane_across_workspaces(&PaneRef::Id(from_pane))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("sender pane {from_pane} not found"),
                )
            })?;
        let (target_ws, target_id) = match self.resolve_pane_across_workspaces(target) {
            Some(pair) => pair,
            None => return Ok(ipc::PeerSendOutcome::Delivered),
        };
        if sender_ws != target_ws {
            return Ok(ipc::PeerSendOutcome::Delivered);
        }
        if let Some(outcome) = self.duplicate_peer_send_outcome(target_id, from_pane, &body) {
            // Same (target, from, body) within the dedupe window —
            // treat as a no-op so duplicate dispatcher acks /
            // worker false-fires don't paper the receiver's
            // transcript with phantom Human: turns. The sender
            // gets a successful Ok() reply so it can't probe the
            // dedupe state. (renga#221)
            return Ok(outcome);
        }
        self.materialize_unfocused_codex_peer_notification();
        let from_name = self.workspaces[sender_ws]
            .pane_names
            .iter()
            .find(|(_, id)| **id == from_pane)
            .map(|(n, _)| n.clone());
        let from_kind = self.peer_client_kinds.get(&from_pane).copied();
        if self.peer_delivery_ready.contains(&target_id)
            && self.pane_expects_codex_peer_delivery(target_ws, target_id)
        {
            let message = PendingCodexPeerMessage {
                from_pane,
                from_name: from_name.clone(),
                from_kind,
            };
            let target_is_focused = self.active_tab == target_ws
                && self.workspaces[target_ws].focus_target == FocusTarget::Pane
                && self.workspaces[target_ws].focused_pane_id == target_id;
            let nudge_commit_in_flight = self
                .pending_codex_peer_messages
                .get(&target_id)
                .and_then(|queue| queue.front())
                .is_some_and(|delivery| {
                    matches!(delivery, PendingCodexPeerDelivery::QueueAt { .. })
                });
            if target_is_focused && !nudge_commit_in_flight {
                self.route_focused_codex_peer_message(target_id, message)?;
            } else {
                self.push_pending_codex_peer_nudge(target_id, message);
            }
        }
        let message = PendingPeerInboxMessage {
            from_pane,
            from_name,
            from_kind,
            body: body.clone(),
            ts_ms: ipc::events::now_ms(),
        };
        if self.peer_delivery_ready.contains(&target_id) {
            self.emit_peer_inbox(target_id, message);
            let outcome = ipc::PeerSendOutcome::Delivered;
            self.record_peer_send(target_id, from_pane, &body, outcome);
            Ok(outcome)
        } else {
            let queue = self.pending_peer_inbox.entry(target_id).or_default();
            let retained_bytes: usize = queue.iter().map(|item| item.body.len()).sum();
            if queue.len() >= PENDING_PEER_INBOX_MAX_MESSAGES
                || retained_bytes.saturating_add(message.body.len()) > PENDING_PEER_INBOX_MAX_BYTES
            {
                return Err(ipc::CodedError::new(
                    ipc::err_code::PEER_QUEUE_FULL,
                    format!("peer inbox queue for pane {target_id} is full"),
                ));
            }
            queue.push_back(message);
            let outcome = ipc::PeerSendOutcome::Queued;
            self.record_peer_send(target_id, from_pane, &body, outcome);
            Ok(outcome)
        }
    }

    fn emit_peer_inbox(&self, target_pane: usize, message: PendingPeerInboxMessage) {
        self.event_bus.emit(ipc::Event::PeerInbox {
            target_pane,
            from_pane: message.from_pane,
            from_name: message.from_name,
            from_kind: message.from_kind,
            body: message.body,
            ts_ms: message.ts_ms,
        });
    }

    /// Return the original outcome when an identical (target, from, body)
    /// peer send arrived within [`PEER_SEND_DEDUPE_TTL`].
    /// and stale entries (older than the TTL) are evicted on every
    /// call so the map can't grow unbounded under heavy traffic.
    fn duplicate_peer_send_outcome(
        &mut self,
        target: usize,
        from: usize,
        body: &str,
    ) -> Option<ipc::PeerSendOutcome> {
        let now = Instant::now();
        self.recent_peer_sends
            .retain(|_, (ts, _)| now.duration_since(*ts) < PEER_SEND_DEDUPE_TTL);
        let key = (target, from, body.to_string());
        match self.recent_peer_sends.get(&key).copied() {
            Some((prev, outcome)) if now.duration_since(prev) < PEER_SEND_DEDUPE_TTL => {
                // Refresh the timestamp so a chatty sender keeps
                // getting its retries collapsed instead of slipping
                // a duplicate through right at the TTL boundary.
                self.recent_peer_sends.insert(key, (now, outcome));
                Some(outcome)
            }
            _ => None,
        }
    }

    fn record_peer_send(
        &mut self,
        target: usize,
        from: usize,
        body: &str,
        outcome: ipc::PeerSendOutcome,
    ) {
        self.recent_peer_sends
            .insert((target, from, body.to_string()), (Instant::now(), outcome));
    }

    pub(crate) fn handle_peer_register_client(
        &mut self,
        pane_id: usize,
        kind: PeerClientKind,
    ) -> std::result::Result<(), ipc::CodedError> {
        self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("pane {pane_id} not found for peer registration"),
                )
            })?;
        self.peer_client_kinds.insert(pane_id, kind);
        Ok(())
    }

    pub(crate) fn handle_peer_set_ready(
        &mut self,
        pane_id: usize,
        kind: PeerClientKind,
        ready: bool,
    ) -> std::result::Result<(), ipc::CodedError> {
        self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("pane {pane_id} not found for peer readiness"),
                )
            })?;
        if !ready {
            self.peer_delivery_ready.remove(&pane_id);
            return Ok(());
        }
        // Readiness and kind travel atomically so a failed earlier metadata
        // registration cannot suppress Codex nudge setup.
        self.peer_client_kinds.insert(pane_id, kind);
        self.peer_delivery_ready.insert(pane_id);
        if let Some(messages) = self.pending_peer_inbox.remove(&pane_id) {
            if self.peer_client_kinds.get(&pane_id) == Some(&PeerClientKind::Codex) {
                if let Some(last) = messages.back() {
                    self.push_pending_codex_peer_nudge(
                        pane_id,
                        PendingCodexPeerMessage {
                            from_pane: last.from_pane,
                            from_name: last.from_name.clone(),
                            from_kind: last.from_kind,
                        },
                    );
                }
            }
            for message in messages {
                self.emit_peer_inbox(pane_id, message);
            }
        }
        Ok(())
    }

    fn push_pending_codex_peer_nudge(&mut self, pane_id: usize, message: PendingCodexPeerMessage) {
        let queue = self.pending_codex_peer_messages.entry(pane_id).or_default();
        if queue.is_empty() {
            queue.push_back(PendingCodexPeerDelivery::Draft(message));
        }
    }

    fn show_codex_peer_notification(&mut self, pane_id: usize, message: PendingCodexPeerMessage) {
        self.pending_codex_peer_messages.remove(&pane_id);
        match self.codex_peer_notification.as_mut() {
            Some(notification) if notification.target_pane == pane_id => {
                notification.register_message(message);
            }
            _ => {
                self.codex_peer_notification = Some(CodexPeerNotificationState {
                    target_pane: pane_id,
                    message,
                    pending_count: 1,
                });
            }
        }
        self.dirty = true;
    }

    fn route_focused_codex_peer_message(
        &mut self,
        pane_id: usize,
        message: PendingCodexPeerMessage,
    ) -> std::result::Result<(), ipc::CodedError> {
        let registered_codex = self.peer_client_kinds.get(&pane_id) == Some(&PeerClientKind::Codex);
        let has_draft = self
            .ws()
            .panes
            .get(&pane_id)
            .and_then(codex_composer_has_draft)
            .unwrap_or(false);
        if has_draft {
            self.show_codex_peer_notification(pane_id, message);
            return Ok(());
        }
        let ready = self
            .ws()
            .panes
            .get(&pane_id)
            .is_some_and(|pane| Self::codex_peer_delivery_ready(registered_codex, pane));
        if !ready {
            self.push_pending_codex_peer_nudge(pane_id, message);
            return Ok(());
        }
        let payload = crate::mcp_peer::build_send_keys_payload(
            &format_codex_peer_message(&message),
            None,
            false,
        )
        .expect("codex peer draft payload");
        let pane =
            self.ws_mut().panes.get_mut(&pane_id).ok_or_else(|| {
                ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished")
            })?;
        write_input_to_pane(pane, payload.as_bytes(), false)?;
        let queue = self.pending_codex_peer_messages.entry(pane_id).or_default();
        queue.clear();
        queue.push_back(PendingCodexPeerDelivery::SubmitAt(
            Instant::now() + CODEX_PEER_NUDGE_COMMIT_DELAY,
        ));
        self.codex_peer_notification = None;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn codex_peer_notification_is_visible(&self) -> bool {
        if self.overlay.is_some() {
            return false;
        }
        let Some(notification) = self.codex_peer_notification.as_ref() else {
            return false;
        };
        self.ws().focus_target == FocusTarget::Pane
            && self.ws().focused_pane_id == notification.target_pane
            && self.ws().panes.contains_key(&notification.target_pane)
    }

    pub(crate) fn visible_codex_peer_notification(&self) -> Option<&CodexPeerNotificationState> {
        self.codex_peer_notification_is_visible()
            .then_some(self.codex_peer_notification.as_ref())
            .flatten()
    }

    pub(crate) fn dismiss_codex_peer_notification(&mut self) {
        if self.codex_peer_notification.take().is_some() {
            self.dirty = true;
        }
    }

    pub(crate) fn requeue_codex_peer_notification(&mut self) {
        let Some(notification) = self.codex_peer_notification.take() else {
            return;
        };
        self.push_pending_codex_peer_nudge(notification.target_pane, notification.message);
        self.dirty = true;
    }

    fn materialize_unfocused_codex_peer_notification(&mut self) {
        let Some(notification) = self.codex_peer_notification.clone() else {
            return;
        };
        if self.codex_peer_notification_is_visible() {
            return;
        }
        if self
            .resolve_pane_across_workspaces(&PaneRef::Id(notification.target_pane))
            .is_some()
        {
            self.push_pending_codex_peer_nudge(notification.target_pane, notification.message);
        }
        self.codex_peer_notification = None;
        self.dirty = true;
    }

    pub(crate) fn accept_codex_peer_notification(
        &mut self,
    ) -> std::result::Result<bool, ipc::CodedError> {
        let Some(notification) = self.codex_peer_notification.clone() else {
            return Ok(false);
        };
        if !self.codex_peer_notification_is_visible() {
            return Ok(false);
        }
        let payload = crate::mcp_peer::build_send_keys_payload(
            &format_codex_peer_message(&notification.message),
            None,
            false,
        )
        .expect("codex peer notification payload");
        let pane = self
            .ws_mut()
            .panes
            .get_mut(&notification.target_pane)
            .ok_or_else(|| ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished"))?;
        write_input_to_pane(pane, payload.as_bytes(), false)?;
        self.pending_codex_peer_messages
            .remove(&notification.target_pane);
        self.codex_peer_notification = None;
        self.dirty = true;
        Ok(true)
    }

    pub(crate) fn pane_expects_codex_peer_delivery(&self, ws_index: usize, pane_id: usize) -> bool {
        // Registration is authoritative when present. Without this
        // short-circuit a Claude-registered pane whose current OSC
        // title transiently contains the substring "codex" (very
        // common for orchestration workers debugging Codex-related
        // issues) would fall through to the title heuristic and be
        // mis-classified as a Codex recipient — see issue #209's
        // discussion of the related #208 regression.
        match self.peer_client_kinds.get(&pane_id) {
            Some(PeerClientKind::Codex) => return true,
            Some(PeerClientKind::Claude) => return false,
            None => {}
        }
        self.workspaces[ws_index]
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.is_codex_running() || pending_startup_looks_like_codex(pane))
    }

    pub(crate) fn codex_peer_delivery_ready(registered_codex: bool, pane: &Pane) -> bool {
        if !registered_codex && !pane.is_codex_running() {
            return false;
        }
        let Ok(parser) = pane.parser.lock() else {
            return false;
        };
        codex_peer_screen_snapshot(parser.screen()).ready_for_nudge
    }

    fn codex_peer_screen_snapshot(
        registered_codex: bool,
        pane: &Pane,
    ) -> Option<CodexPeerScreenSnapshot> {
        if !registered_codex && !pane.is_codex_running() {
            return None;
        }
        let Ok(parser) = pane.parser.lock() else {
            return None;
        };
        Some(codex_peer_screen_snapshot(parser.screen()))
    }

    pub(crate) fn flush_pending_codex_peer_messages(&mut self) {
        self.materialize_unfocused_codex_peer_notification();
        let now = Instant::now();
        let mut empty_panes = Vec::new();
        let mut focused_notifications = Vec::new();
        let active_tab = self.active_tab;
        for (ws_index, ws) in self.workspaces.iter_mut().enumerate() {
            let pane_ids: Vec<usize> = ws.panes.keys().copied().collect();
            for pane_id in pane_ids {
                let pane_is_focused = ws_index == active_tab
                    && ws.focus_target == FocusTarget::Pane
                    && ws.focused_pane_id == pane_id;
                let Some(queue) = self.pending_codex_peer_messages.get_mut(&pane_id) else {
                    continue;
                };
                let Some(delivery) = queue.front().cloned() else {
                    empty_panes.push(pane_id);
                    continue;
                };
                if let Some(pane) = ws.panes.get_mut(&pane_id) {
                    let registered_codex =
                        self.peer_client_kinds.get(&pane_id) == Some(&PeerClientKind::Codex);
                    let screen = Self::codex_peer_screen_snapshot(registered_codex, pane);
                    match delivery {
                        PendingCodexPeerDelivery::Draft(message) => {
                            if screen.as_ref().and_then(|state| state.has_draft) == Some(true) {
                                if pane_is_focused {
                                    queue.pop_front();
                                    focused_notifications.push((pane_id, message));
                                    self.dirty = true;
                                }
                                continue;
                            }
                            let payload_text = format_codex_peer_message(&message);
                            if !pane_is_focused
                                && screen.as_ref().is_some_and(|state| state.can_queue_message)
                            {
                                let payload = crate::mcp_peer::build_send_keys_payload(
                                    &payload_text,
                                    None,
                                    false,
                                )
                                .expect("codex peer draft payload");
                                if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                    queue.pop_front();
                                    queue.push_front(PendingCodexPeerDelivery::QueueAt {
                                        ready_at: now + CODEX_PEER_NUDGE_COMMIT_DELAY,
                                        expires_at: now + CODEX_PEER_NUDGE_COMMIT_TIMEOUT,
                                        message,
                                        expected_composer: normalize_codex_composer_expected(
                                            &payload_text,
                                        ),
                                    });
                                    self.dirty = true;
                                }
                                continue;
                            }
                            if !screen.as_ref().is_some_and(|state| state.ready_for_nudge) {
                                continue;
                            }
                            let payload = crate::mcp_peer::build_send_keys_payload(
                                &payload_text,
                                None,
                                false,
                            )
                            .expect("codex peer draft payload");
                            if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                queue.pop_front();
                                queue.push_front(PendingCodexPeerDelivery::SubmitAt(
                                    now + CODEX_PEER_NUDGE_COMMIT_DELAY,
                                ));
                                self.dirty = true;
                            }
                        }
                        PendingCodexPeerDelivery::SubmitAt(ready_at) => {
                            if now < ready_at {
                                continue;
                            }
                            let payload = crate::mcp_peer::build_send_keys_payload("", None, true)
                                .expect("codex peer submit payload");
                            if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                queue.pop_front();
                                self.dirty = true;
                            }
                        }
                        PendingCodexPeerDelivery::QueueAt {
                            ready_at,
                            expires_at,
                            message,
                            expected_composer,
                        } => {
                            let composer_matches =
                                screen.as_ref().and_then(|state| state.composer.as_ref())
                                    == Some(&expected_composer);
                            if pane_is_focused {
                                if composer_matches {
                                    let _ = write_input_to_pane(pane, b"\x15", false);
                                }
                                queue.pop_front();
                                focused_notifications.push((pane_id, message));
                                self.dirty = true;
                                continue;
                            }
                            if now >= expires_at {
                                if composer_matches {
                                    let _ = write_input_to_pane(pane, b"\x15", false);
                                }
                                queue.pop_front();
                                queue.push_front(PendingCodexPeerDelivery::Draft(message));
                                self.dirty = true;
                                continue;
                            }
                            if now < ready_at || !composer_matches {
                                continue;
                            }
                            let payload = if screen
                                .as_ref()
                                .is_some_and(|state| state.busy_queue_available)
                            {
                                b"\t".as_slice()
                            } else if screen
                                .as_ref()
                                .is_some_and(|state| state.can_submit_message)
                            {
                                b"\r".as_slice()
                            } else {
                                continue;
                            };
                            if write_input_to_pane(pane, payload, false).is_ok() {
                                queue.pop_front();
                                self.dirty = true;
                            }
                        }
                    }
                }
                if queue.is_empty() {
                    empty_panes.push(pane_id);
                }
            }
        }
        for pane_id in empty_panes {
            self.pending_codex_peer_messages.remove(&pane_id);
        }
        for (pane_id, message) in focused_notifications {
            self.show_codex_peer_notification(pane_id, message);
        }
    }
}
