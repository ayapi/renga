use super::*;

pub(crate) const CODEX_APPEND_ENTER_DELAY: Duration = Duration::from_millis(75);
pub(crate) const CODEX_PEER_NUDGE_COMMIT_DELAY: Duration = Duration::from_millis(1000);
pub(crate) const CODEX_PEER_NUDGE_COMMIT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const CODEX_PEER_NUDGE_MAX_RETRIES: u8 = 1;
static CODEX_PEER_DEBUG_RECORD_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
thread_local! {
    static CODEX_PEER_DEBUG_LOG_PATH_TEST_OVERRIDE:
        std::cell::RefCell<Option<Option<std::ffi::OsString>>> =
        const { std::cell::RefCell::new(Some(None)) };
}

#[cfg(test)]
pub(crate) fn set_codex_peer_debug_log_path_test_override(
    value: Option<Option<std::ffi::OsString>>,
) {
    CODEX_PEER_DEBUG_LOG_PATH_TEST_OVERRIDE.with(|current| *current.borrow_mut() = value);
}

pub(crate) fn codex_peer_debug_log_path() -> Option<std::ffi::OsString> {
    resolve_codex_peer_debug_log_path(|| std::env::var_os("RENGA_DEBUG_CODEX_PEER_LOG"))
}

fn resolve_codex_peer_debug_log_path(
    read_env: impl FnOnce() -> Option<std::ffi::OsString>,
) -> Option<std::ffi::OsString> {
    #[cfg(test)]
    if let Some(path) = CODEX_PEER_DEBUG_LOG_PATH_TEST_OVERRIDE.with(|value| value.borrow().clone())
    {
        return path;
    }

    read_env()
}
// A 1.5-second grace period spans many 30-fps redraws, so transient partial
// frames can settle while a genuinely stalled delivery still becomes visible.
pub(crate) const CODEX_PEER_DRAFT_STALL_TIMEOUT: Duration = Duration::from_millis(1500);
#[cfg(test)]
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
/// Receipt retries are cheap in the normal millisecond-scale path and
/// make best-effort event-bus drops recoverable without duplicating the
/// receiver's inbox entry.
pub(crate) const PEER_INBOX_ACK_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Must expire before IPC's five-second App reply limit so the sender
/// receives a specific delivery error instead of the generic App timeout.
pub(crate) const PEER_INBOX_ACK_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingPeerInboxMessage {
    pub(crate) from_pane: usize,
    pub(crate) from_name: Option<String>,
    pub(crate) from_kind: Option<PeerClientKind>,
    pub(crate) body: String,
    pub(crate) ts_ms: u64,
}

#[derive(Debug)]
pub(crate) struct PendingPeerInboxDelivery {
    pub(crate) target_pane: usize,
    pub(crate) message: PendingPeerInboxMessage,
    pub(crate) nudge: Option<PendingCodexPeerMessage>,
    pub(crate) replies:
        Vec<oneshot::Sender<std::result::Result<ipc::PeerSendOutcome, ipc::CodedError>>>,
    pub(crate) next_retry_at: Instant,
    pub(crate) expires_at: Instant,
}

enum PreparedPeerSend {
    Immediate(ipc::PeerSendOutcome),
    Confirm {
        target_pane: usize,
        message: PendingPeerInboxMessage,
        nudge: Option<PendingCodexPeerMessage>,
    },
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
    pub(crate) retries_remaining: Option<u8>,
}

impl CodexPeerNotificationState {
    fn register_message(
        &mut self,
        message: PendingCodexPeerMessage,
        retries_remaining: Option<u8>,
    ) {
        self.message = message;
        self.pending_count = self.pending_count.saturating_add(1);
        self.retries_remaining = self.retries_remaining.max(retries_remaining);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingCodexPeerDelivery {
    Draft {
        message: PendingCodexPeerMessage,
        retries_remaining: u8,
        stalled_since: Instant,
        delivery_sequence: Option<u64>,
    },
    SubmitAt {
        ready_at: Instant,
        expected_composer: String,
        expected_composer_raw: Option<String>,
        delivery_sequence: Option<u64>,
    },
    QueueAt {
        ready_at: Instant,
        expires_at: Instant,
        message: PendingCodexPeerMessage,
        expected_composer: String,
        expected_composer_raw: Option<String>,
        delivery_sequence: Option<u64>,
        retries_remaining: u8,
    },
    AwaitFocus {
        message: PendingCodexPeerMessage,
        retries_remaining: u8,
        delivery_sequence: Option<u64>,
    },
}

impl PendingCodexPeerDelivery {
    fn delivery_sequence(&self) -> Option<u64> {
        match self {
            Self::Draft {
                delivery_sequence, ..
            }
            | Self::SubmitAt {
                delivery_sequence, ..
            }
            | Self::QueueAt {
                delivery_sequence, ..
            }
            | Self::AwaitFocus {
                delivery_sequence, ..
            } => *delivery_sequence,
        }
    }
}

#[derive(Debug)]
struct CodexPeerScreenSnapshot {
    has_draft: Option<bool>,
    composer: Option<String>,
    ready_for_nudge: bool,
    can_queue_message: bool,
    hide_cursor: bool,
    native_queue_busy: bool,
    busy_queue_available: bool,
    can_submit_injected_message: bool,
    debug: Option<CodexPeerDebugScreenSnapshot>,
}

#[derive(Debug)]
struct CodexPeerDebugScreenSnapshot {
    composer_raw: Option<String>,
    status_raw: String,
    footer_raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPeerDebugObservation {
    action: &'static str,
    delivery_sequence: Option<u64>,
    composer: Option<String>,
    composer_matches: Option<bool>,
    has_draft: Option<bool>,
    ready_for_nudge: Option<bool>,
    can_queue_message: Option<bool>,
    hide_cursor: Option<bool>,
    native_queue_busy: Option<bool>,
    busy_queue_available: Option<bool>,
    can_submit_injected_message: Option<bool>,
}

#[cfg(test)]
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

fn looks_like_codex_footer_rows(rows: &[String], separator_rows: usize) -> bool {
    let joined = rows.concat();
    // Position alone is insufficient when transcript output reaches the last
    // screen row. Match footer metadata instead; unfamiliar future formats
    // remain safe because a stalled Draft surfaces through AwaitFocus.
    let model_footer = rows.len() == 1
        && ["minimal", "low", "medium", "high", "xhigh"]
            .iter()
            .any(|effort| joined.contains(&format!("{effort}\u{b7}")));
    (separator_rows == 1 && model_footer)
        // Codex v0.147.0 no longer shows this footer, but retain exact support
        // for older releases where the phrase may wrap in a narrow pane.
        || joined == "entertosend"
        || (joined.starts_with("tabtoqueuemessage") && joined.ends_with("contextleft"))
}

fn codex_live_composer_position(screen: &vt100::Screen) -> Option<(u16, u16)> {
    let (rows, cols) = screen.size();
    let (prompt_row, prompt_col) = (0..rows).rev().find_map(|row| {
        (0..cols).find_map(|col| {
            screen
                .cell(row, col)
                .is_some_and(|cell| cell.contents() == "\u{203a}")
                .then_some((row, col))
        })
    })?;
    let (cursor_row, _) = screen.cursor_position();
    // Codex v0.147.0 keeps the live composer as the final prompt marker,
    // followed only by wrapped input, blank separation, and its footer. If a
    // future layout adds unknown content there, leave the peer nudge pending.
    let mut separator_seen = false;
    let mut separator_rows = 0;
    let mut content_before_separator = false;
    let mut last_content_before_separator = prompt_row;
    let mut footer_rows = Vec::new();

    for row in prompt_row.saturating_add(1)..rows {
        let normalized = normalized_screen_rows(screen, row, row.saturating_add(1));
        if normalized.is_empty() {
            if footer_rows.is_empty() {
                separator_seen = true;
                separator_rows += 1;
            }
            continue;
        }
        if !separator_seen {
            content_before_separator = true;
            last_content_before_separator = row;
            continue;
        }
        footer_rows.push(normalized);
    }

    let footer_seen = looks_like_codex_footer_rows(&footer_rows, separator_rows);
    if footer_seen
        || (footer_rows.is_empty() && !content_before_separator && cursor_row == prompt_row)
    {
        return Some((prompt_row, prompt_col));
    }
    (footer_rows.is_empty()
        && content_before_separator
        && cursor_row > prompt_row
        && cursor_row <= last_content_before_separator)
        .then_some((prompt_row, prompt_col))
}

pub(crate) fn codex_prompt_allows_peer_nudge_on_screen(screen: &vt100::Screen) -> Option<bool> {
    if screen.hide_cursor() {
        return Some(false);
    }
    let (_, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let (prompt_row, prompt_col) = codex_live_composer_position(screen)?;
    if cursor_row > prompt_row && !cursor_is_on_codex_footer(screen, prompt_row, cursor_row, cols) {
        return Some(false);
    }
    if cursor_row == prompt_row
        && cursor_col > codex_composer_editable_start(screen, prompt_row, prompt_col)
    {
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

fn codex_composer_editable_start(screen: &vt100::Screen, row: u16, prompt_col: u16) -> u16 {
    let input_start = prompt_col.saturating_add(1);
    if screen
        .cell(row, input_start)
        .is_some_and(|cell| cell.contents().trim().is_empty())
    {
        input_start.saturating_add(1)
    } else {
        input_start
    }
}

pub(crate) fn codex_composer_has_draft_on_screen(screen: &vt100::Screen) -> Option<bool> {
    let (_, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let (row, prompt_col) = codex_live_composer_position(screen)?;
    let input_start = prompt_col.saturating_add(1);
    let editable_start = codex_composer_editable_start(screen, row, prompt_col);
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
    Some(has_normal_input_text)
}

fn raw_codex_composer_text(screen: &vt100::Screen) -> Option<String> {
    let (rows, cols) = screen.size();
    let (prompt_row, prompt_col) = codex_live_composer_position(screen)?;
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
        if row > prompt_row {
            text.push('\n');
        }
        text.push_str(&line);
    }
    Some(text)
}

pub(crate) fn normalized_codex_composer_text(screen: &vt100::Screen) -> Option<String> {
    let (rows, cols) = screen.size();
    let (prompt_row, prompt_col) = codex_live_composer_position(screen)?;
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
    (start..end)
        .map(|row| {
            let mut text = String::new();
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col) {
                    text.push_str(cell.contents());
                }
            }
            text.chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn raw_screen_rows(screen: &vt100::Screen, start: u16, end: u16) -> String {
    let (_, cols) = screen.size();
    (start..end)
        .map(|row| {
            let mut text = String::new();
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col) {
                    text.push_str(cell.contents());
                }
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn analyze_codex_peer_screen(
    screen: &vt100::Screen,
    capture_debug: bool,
) -> CodexPeerScreenSnapshot {
    let has_draft = codex_composer_has_draft_on_screen(screen);
    let composer_raw = capture_debug
        .then(|| raw_codex_composer_text(screen))
        .flatten();
    let composer = if capture_debug {
        composer_raw
            .as_deref()
            .map(normalize_codex_composer_expected)
    } else {
        normalized_codex_composer_text(screen)
    };
    let (rows, _) = screen.size();
    let prompt_row = codex_live_composer_position(screen).map(|(row, _)| row);
    let (status, footer) = prompt_row.map_or_else(
        || (String::new(), String::new()),
        |prompt_row| {
            // Codex v0.147.0 keeps the busy status three rows above the
            // prompt (status, two blank rows, prompt). Four rows retain one
            // row of tolerance; remeasure this if Codex changes that layout.
            let status_start = prompt_row.saturating_sub(4);
            // With a roughly 200-character nudge in a 56-column pane, the
            // native queue footer was five rows below the prompt. Seven rows
            // below the prompt cover the measured wrapping with room to spare.
            let footer_end = prompt_row.saturating_add(8).min(rows);
            (
                normalized_screen_rows(screen, status_start, prompt_row),
                normalized_screen_rows(screen, prompt_row.saturating_add(1), footer_end),
            )
        },
    );
    let (status_raw, footer_raw) = if capture_debug {
        prompt_row.map_or_else(
            || (String::new(), String::new()),
            |prompt_row| {
                let status_start = prompt_row.saturating_sub(4);
                let footer_end = prompt_row.saturating_add(8).min(rows);
                (
                    raw_screen_rows(screen, status_start, prompt_row),
                    raw_screen_rows(screen, prompt_row.saturating_add(1), footer_end),
                )
            },
        )
    } else {
        (String::new(), String::new())
    };
    // Positive detection controls whether renga may inject and press Tab, so
    // anchor the busy signal above the composer and the queue action below it.
    // Transcript mentions and unknown future UI safely remain pending.
    let native_queue_busy = status.lines().any(|line| {
        let status_text = line.trim_start_matches(|ch: char| !ch.is_alphanumeric());
        status_text.starts_with("working(") && status_text.contains("esctointerrupt")
    });
    // A partial or unfamiliar interrupt status is not enough evidence to use
    // Codex's native queue, but it is enough to reject the idle path. This
    // keeps wrapped or renamed status text from causing an Enter mid-turn.
    let interrupt_status_visible = status.replace('\n', "").contains("esctointerrupt");
    let busy_queue_available =
        !screen.hide_cursor() && native_queue_busy && footer.contains("tabtoqueuemessage");
    let can_queue_message = !screen.hide_cursor() && has_draft == Some(false) && native_queue_busy;
    // Codex does not render an idle action hint. After injection, the reliable
    // completion signal is that the prompt remains visible while the busy
    // status above it has disappeared. QueueAt separately requires the exact
    // injected composer text before this may result in Enter.
    let can_submit_injected_message =
        prompt_row.is_some() && !screen.hide_cursor() && !interrupt_status_visible;
    let ready_without_prompt = prompt_row.is_none() && {
        let screen_text = normalized_screen_rows(screen, 0, rows);
        screen_text.contains("entertosend") || screen_text.contains("readyforinput")
    };
    let ready_for_nudge = !interrupt_status_visible
        && screen_has_visible_text(screen)
        && (codex_prompt_allows_peer_nudge_on_screen(screen).unwrap_or(ready_without_prompt));
    CodexPeerScreenSnapshot {
        has_draft,
        composer,
        ready_for_nudge,
        can_queue_message,
        hide_cursor: screen.hide_cursor(),
        native_queue_busy,
        busy_queue_available,
        can_submit_injected_message,
        debug: capture_debug.then_some(CodexPeerDebugScreenSnapshot {
            composer_raw,
            status_raw,
            footer_raw,
        }),
    }
}

fn instant_offset_millis(now: Instant, target: Instant) -> i128 {
    if now >= target {
        now.duration_since(target).as_millis() as i128
    } else {
        -(target.duration_since(now).as_millis() as i128)
    }
}

struct CodexPeerDecision<'a> {
    pane_id: usize,
    delivery_sequence: Option<u64>,
    now: Instant,
    ready_at: Option<Instant>,
    expires_at: Option<Instant>,
    expected_composer: Option<&'a str>,
    expected_composer_raw: Option<&'a str>,
    screen: Option<&'a CodexPeerScreenSnapshot>,
    composer_matches: Option<bool>,
    retries_remaining: Option<u8>,
    queue_entries_total: usize,
    other_queue_entries: usize,
    action: &'static str,
}

impl CodexPeerDebugObservation {
    fn from_decision(decision: &CodexPeerDecision<'_>) -> Self {
        let screen = decision.screen;
        Self {
            action: decision.action,
            delivery_sequence: decision.delivery_sequence,
            composer: screen.and_then(|state| state.composer.clone()),
            composer_matches: decision.composer_matches,
            has_draft: screen.and_then(|state| state.has_draft),
            ready_for_nudge: screen.map(|state| state.ready_for_nudge),
            can_queue_message: screen.map(|state| state.can_queue_message),
            hide_cursor: screen.map(|state| state.hide_cursor),
            native_queue_busy: screen.map(|state| state.native_queue_busy),
            busy_queue_available: screen.map(|state| state.busy_queue_available),
            can_submit_injected_message: screen.map(|state| state.can_submit_injected_message),
        }
    }
}

pub(super) fn append_codex_peer_debug_record(
    path: &std::ffi::OsStr,
    mut record: serde_json::Value,
) {
    use std::io::Write;

    let record_sequence =
        CODEX_PEER_DEBUG_RECORD_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let timestamp_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    if let Some(object) = record.as_object_mut() {
        object.insert(
            "timestamp_unix_ms".to_string(),
            serde_json::json!(timestamp_unix_ms),
        );
        object.insert(
            "process_id".to_string(),
            serde_json::json!(std::process::id()),
        );
        object.insert(
            "record_sequence".to_string(),
            serde_json::json!(record_sequence),
        );
    }
    let Ok(mut line) = serde_json::to_vec(&record) else {
        return;
    };
    line.push(b'\n');
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::PathBuf::from(path))
    {
        let _ = file.write_all(&line);
    }
}

fn log_codex_peer_kind_update(
    pane_id: usize,
    old_kind: Option<PeerClientKind>,
    new_kind: PeerClientKind,
    update_path: &'static str,
) {
    let Some(path) = codex_peer_debug_log_path() else {
        return;
    };
    let kind_label = |kind| match kind {
        PeerClientKind::Claude => "claude",
        PeerClientKind::Codex => "codex",
    };
    append_codex_peer_debug_record(
        &path,
        serde_json::json!({
            "action": "client_kind_updated",
            "pane_id": pane_id,
            "kind_update_path": update_path,
            "old_client_kind": old_kind.map(kind_label),
            "new_client_kind": kind_label(new_kind),
            "receive_mode": match new_kind.receive_mode() {
                ipc::PeerReceiveMode::Push => "push",
                ipc::PeerReceiveMode::Pull => "pull",
            },
        }),
    );
}

fn log_codex_peer_decision(path: &std::ffi::OsStr, decision: CodexPeerDecision<'_>) {
    let CodexPeerDecision {
        pane_id,
        delivery_sequence,
        now,
        ready_at,
        expires_at,
        expected_composer,
        expected_composer_raw,
        screen,
        composer_matches,
        retries_remaining,
        queue_entries_total,
        other_queue_entries,
        action,
    } = decision;
    let timing = match (ready_at, expires_at) {
        (_, Some(expires_at)) if now >= expires_at => Some("expired"),
        (Some(ready_at), _) if now < ready_at => Some("waiting_ready_at"),
        (Some(_), Some(_)) => Some("ready_before_expiry"),
        (Some(_), None) => Some("ready"),
        (None, _) => None,
    };
    let debug = screen.and_then(|state| state.debug.as_ref());
    append_codex_peer_debug_record(
        path,
        serde_json::json!({
            "pane_id": pane_id,
            "delivery_sequence": delivery_sequence,
            "composer_matches": composer_matches,
            "retries_remaining": retries_remaining,
            "expected_composer": expected_composer,
            "expected_composer_raw": expected_composer_raw,
            "screen_composer": screen.and_then(|state| state.composer.as_deref()),
            "screen_composer_raw": debug.and_then(|state| state.composer_raw.as_deref()),
            "has_draft": screen.and_then(|state| state.has_draft),
            "ready_for_nudge": screen.map(|state| state.ready_for_nudge),
            "can_queue_message": screen.map(|state| state.can_queue_message),
            "hide_cursor": screen.map(|state| state.hide_cursor),
            "native_queue_busy": screen.map(|state| state.native_queue_busy),
            "busy_queue_available": screen.map(|state| state.busy_queue_available),
            "can_submit_injected_message": screen.map(|state| state.can_submit_injected_message),
            "footer_raw": debug.map(|state| state.footer_raw.as_str()),
            "status_raw": debug.map(|state| state.status_raw.as_str()),
            "timing": timing,
            "now_minus_ready_at_ms": ready_at.map(|target| instant_offset_millis(now, target)),
            "now_minus_expires_at_ms": expires_at.map(|target| instant_offset_millis(now, target)),
            "queue_entries_total": queue_entries_total,
            "other_queue_entries": other_queue_entries,
            "action": action,
        }),
    );
}

fn log_codex_peer_decision_if_changed(
    path: &std::ffi::OsStr,
    observations: &mut HashMap<usize, CodexPeerDebugObservation>,
    decision: CodexPeerDecision<'_>,
) {
    let observation = CodexPeerDebugObservation::from_decision(&decision);
    if observations.get(&decision.pane_id) == Some(&observation) {
        return;
    }
    observations.insert(decision.pane_id, observation);
    log_codex_peer_decision(path, decision);
}

fn codex_composer_has_draft(pane: &Pane) -> Option<bool> {
    let lock_started = super::frame_diagnostics::lock_wait_started();
    let Ok(parser) = pane.parser.lock() else {
        return None;
    };
    super::frame_diagnostics::record_lock_wait(pane.id, lock_started);
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
    super::frame_diagnostics::record_pty_write(pane.id);
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
    #[cfg(test)]
    pub(crate) fn handle_peer_send(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
    ) -> std::result::Result<ipc::PeerSendOutcome, ipc::CodedError> {
        match self.prepare_peer_send(from_pane, target, body)? {
            PreparedPeerSend::Immediate(outcome) => Ok(outcome),
            PreparedPeerSend::Confirm {
                target_pane,
                message,
                nudge,
            } => {
                // Direct App tests and in-process callers have no MCP
                // process to acknowledge the event. Preserve that helper's
                // synchronous contract while production IPC uses
                // `begin_peer_send` below.
                self.emit_peer_inbox(target_pane, None, message.clone());
                self.finish_confirmed_peer_delivery(target_pane, message, nudge)
            }
        }
    }

    fn prepare_peer_send(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
    ) -> std::result::Result<PreparedPeerSend, ipc::CodedError> {
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
            None => return Ok(PreparedPeerSend::Immediate(ipc::PeerSendOutcome::Delivered)),
        };
        if sender_ws != target_ws {
            return Ok(PreparedPeerSend::Immediate(ipc::PeerSendOutcome::Delivered));
        }
        if let Some(outcome) = self.duplicate_peer_send_outcome(target_id, from_pane, &body) {
            // Same (target, from, body) within the dedupe window —
            // treat as a no-op so duplicate dispatcher acks /
            // worker false-fires don't paper the receiver's
            // transcript with phantom Human: turns. The sender
            // gets a successful Ok() reply so it can't probe the
            // dedupe state. (renga#221)
            return Ok(PreparedPeerSend::Immediate(outcome));
        }
        self.materialize_unfocused_codex_peer_notification();
        let from_name = self.workspaces[sender_ws]
            .pane_names
            .iter()
            .find(|(_, id)| **id == from_pane)
            .map(|(n, _)| n.clone());
        let from_kind = self.peer_client_kinds.get(&from_pane).copied();
        let nudge = if self.peer_delivery_ready.contains(&target_id)
            && self.pane_expects_codex_peer_delivery(target_ws, target_id)
        {
            Some(PendingCodexPeerMessage {
                from_pane,
                from_name: from_name.clone(),
                from_kind,
            })
        } else {
            None
        };
        let message = PendingPeerInboxMessage {
            from_pane,
            from_name,
            from_kind,
            body: body.clone(),
            ts_ms: ipc::events::now_ms(),
        };
        if self.peer_delivery_ready.contains(&target_id) {
            Ok(PreparedPeerSend::Confirm {
                target_pane: target_id,
                message,
                nudge,
            })
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
            Ok(PreparedPeerSend::Immediate(outcome))
        }
    }

    pub(crate) fn begin_peer_send(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
        reply: oneshot::Sender<std::result::Result<ipc::PeerSendOutcome, ipc::CodedError>>,
    ) {
        let target_id = self
            .resolve_pane_across_workspaces(target)
            .map(|(_, id)| id);
        if let Some(pending) = self.pending_peer_deliveries.values_mut().find(|pending| {
            Some(pending.target_pane) == target_id
                && pending.message.from_pane == from_pane
                && pending.message.body == body
        }) {
            pending.replies.push(reply);
            return;
        }
        match self.prepare_peer_send(from_pane, target, body) {
            Ok(PreparedPeerSend::Immediate(outcome)) => {
                let _ = reply.send(Ok(outcome));
            }
            Ok(PreparedPeerSend::Confirm {
                target_pane,
                message,
                nudge,
            }) => self.start_peer_delivery(target_pane, message, nudge, Some(reply)),
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn start_peer_delivery(
        &mut self,
        target_pane: usize,
        message: PendingPeerInboxMessage,
        nudge: Option<PendingCodexPeerMessage>,
        reply: Option<oneshot::Sender<std::result::Result<ipc::PeerSendOutcome, ipc::CodedError>>>,
    ) {
        let delivery_id = self.next_peer_delivery_id;
        self.next_peer_delivery_id = self.next_peer_delivery_id.saturating_add(1).max(1);
        let now = Instant::now();
        self.pending_peer_deliveries.insert(
            delivery_id,
            PendingPeerInboxDelivery {
                target_pane,
                message: message.clone(),
                nudge,
                replies: reply.into_iter().collect(),
                next_retry_at: now + PEER_INBOX_ACK_RETRY_INTERVAL,
                expires_at: now + PEER_INBOX_ACK_TIMEOUT,
            },
        );
        self.emit_peer_inbox(target_pane, Some(delivery_id), message);
    }

    fn emit_peer_inbox(
        &self,
        target_pane: usize,
        delivery_id: Option<u64>,
        message: PendingPeerInboxMessage,
    ) {
        self.event_bus.emit(ipc::Event::PeerInbox {
            delivery_id,
            target_pane,
            from_pane: message.from_pane,
            from_name: message.from_name,
            from_kind: message.from_kind,
            body: message.body,
            ts_ms: message.ts_ms,
        });
    }

    fn finish_confirmed_peer_delivery(
        &mut self,
        target_pane: usize,
        message: PendingPeerInboxMessage,
        nudge: Option<PendingCodexPeerMessage>,
    ) -> std::result::Result<ipc::PeerSendOutcome, ipc::CodedError> {
        let pending_user_confirmation = if let Some(nudge) = nudge {
            let Some((target_ws, _)) =
                self.resolve_pane_across_workspaces(&PaneRef::Id(target_pane))
            else {
                return Err(ipc::CodedError::new(
                    ipc::err_code::PANE_VANISHED,
                    format!("target pane {target_pane} disappeared before peer receipt"),
                ));
            };
            let target_is_focused = self.active_tab == target_ws
                && self.workspaces[target_ws].focus_target == FocusTarget::Pane
                && self.workspaces[target_ws].focused_pane_id == target_pane;
            let nudge_commit_in_flight = self
                .pending_codex_peer_messages
                .get(&target_pane)
                .and_then(|queue| queue.front())
                .is_some_and(|delivery| {
                    matches!(delivery, PendingCodexPeerDelivery::QueueAt { .. })
                });
            if target_is_focused && !nudge_commit_in_flight {
                self.route_focused_codex_peer_message(target_pane, nudge)?
            } else {
                self.push_pending_codex_peer_nudge(target_pane, nudge);
                false
            }
        } else {
            false
        };
        let outcome = if pending_user_confirmation {
            ipc::PeerSendOutcome::PendingUserConfirmation
        } else {
            ipc::PeerSendOutcome::Delivered
        };
        self.record_peer_send(target_pane, message.from_pane, &message.body, outcome);
        Ok(outcome)
    }

    pub(crate) fn handle_peer_inbox_ack(
        &mut self,
        pane_id: usize,
        delivery_id: u64,
    ) -> std::result::Result<(), ipc::CodedError> {
        let Some(pending) = self.pending_peer_deliveries.get(&delivery_id) else {
            return Ok(());
        };
        if pending.target_pane != pane_id {
            return Err(ipc::CodedError::new(
                ipc::err_code::PROTOCOL,
                format!(
                    "peer receipt {delivery_id} belongs to pane {}, not pane {pane_id}",
                    pending.target_pane
                ),
            ));
        }
        let pending = self.pending_peer_deliveries.remove(&delivery_id).unwrap();
        let result = self.finish_confirmed_peer_delivery(
            pending.target_pane,
            pending.message,
            pending.nudge,
        );
        for reply in pending.replies {
            let _ = reply.send(result.clone());
        }
        Ok(())
    }

    pub(crate) fn flush_pending_peer_deliveries(&mut self) {
        let now = Instant::now();
        let mut expired: Vec<u64> = self
            .pending_peer_deliveries
            .iter()
            .filter_map(|(id, pending)| (now >= pending.expires_at).then_some(*id))
            .collect();
        expired.sort_unstable();
        for delivery_id in expired {
            let Some(pending) = self.pending_peer_deliveries.remove(&delivery_id) else {
                continue;
            };
            if pending.replies.is_empty() {
                self.peer_delivery_ready.remove(&pending.target_pane);
                self.pending_peer_inbox
                    .entry(pending.target_pane)
                    .or_default()
                    .push_back(pending.message);
            } else {
                let error = ipc::CodedError::new(
                    ipc::err_code::PEER_DELIVERY_UNCONFIRMED,
                    format!(
                        "pane {} did not confirm peer message receipt",
                        pending.target_pane
                    ),
                );
                for reply in pending.replies {
                    let _ = reply.send(Err(error.clone()));
                }
            }
        }

        let mut retries: Vec<(u64, usize, PendingPeerInboxMessage)> = self
            .pending_peer_deliveries
            .iter_mut()
            .filter_map(|(id, pending)| {
                if now < pending.next_retry_at {
                    return None;
                }
                pending.next_retry_at = now + PEER_INBOX_ACK_RETRY_INTERVAL;
                Some((*id, pending.target_pane, pending.message.clone()))
            })
            .collect();
        retries.sort_unstable_by_key(|(delivery_id, _, _)| *delivery_id);
        for (delivery_id, target_pane, message) in retries {
            self.emit_peer_inbox(target_pane, Some(delivery_id), message);
        }
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
        let old_kind = self.peer_client_kinds.insert(pane_id, kind);
        log_codex_peer_kind_update(pane_id, old_kind, kind, "register");
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
        let old_kind = self.peer_client_kinds.insert(pane_id, kind);
        log_codex_peer_kind_update(pane_id, old_kind, kind, "set_ready");
        self.peer_delivery_ready.insert(pane_id);
        if let Some(messages) = self.pending_peer_inbox.remove(&pane_id) {
            let count = messages.len();
            let is_codex = self.peer_client_kinds.get(&pane_id) == Some(&PeerClientKind::Codex);
            for (index, message) in messages.into_iter().enumerate() {
                let nudge = (is_codex && index + 1 == count).then(|| PendingCodexPeerMessage {
                    from_pane: message.from_pane,
                    from_name: message.from_name.clone(),
                    from_kind: message.from_kind,
                });
                self.start_peer_delivery(pane_id, message, nudge, None);
            }
        }
        Ok(())
    }

    /// Revoke delivery readiness after the IPC server observes that the
    /// pane's last identified event stream has ended. Client kind is
    /// intentionally irrelevant: pull and push subscribers share the
    /// same transport-liveness requirement.
    pub(crate) fn handle_peer_subscriber_gone(&mut self, pane_id: usize) {
        self.peer_delivery_ready.remove(&pane_id);
        let mut failed: Vec<u64> = self
            .pending_peer_deliveries
            .iter()
            .filter_map(|(id, pending)| (pending.target_pane == pane_id).then_some(*id))
            .collect();
        failed.sort_unstable();
        for delivery_id in failed {
            let Some(pending) = self.pending_peer_deliveries.remove(&delivery_id) else {
                continue;
            };
            if pending.replies.is_empty() {
                self.pending_peer_inbox
                    .entry(pane_id)
                    .or_default()
                    .push_back(pending.message);
            } else {
                let error = ipc::CodedError::new(
                    ipc::err_code::PEER_DELIVERY_UNCONFIRMED,
                    format!("peer event stream for pane {pane_id} disconnected"),
                );
                for reply in pending.replies {
                    let _ = reply.send(Err(error.clone()));
                }
            }
        }
    }

    fn push_pending_codex_peer_nudge(&mut self, pane_id: usize, message: PendingCodexPeerMessage) {
        self.push_pending_codex_peer_nudge_with_retries(
            pane_id,
            message,
            CODEX_PEER_NUDGE_MAX_RETRIES,
        );
    }

    fn push_pending_codex_peer_nudge_with_retries(
        &mut self,
        pane_id: usize,
        message: PendingCodexPeerMessage,
        retries_remaining: u8,
    ) {
        let debug_path = codex_peer_debug_log_path();
        let screen = self.codex_peer_debug_screen_snapshot(pane_id, debug_path.is_some());
        let expected_composer_raw = debug_path
            .as_ref()
            .map(|_| format_codex_peer_message(&message));
        let expected_composer = expected_composer_raw
            .as_deref()
            .map(normalize_codex_composer_expected);
        let queue_was_empty = self
            .pending_codex_peer_messages
            .get(&pane_id)
            .is_none_or(VecDeque::is_empty);
        let delivery_sequence = if queue_was_empty && debug_path.is_some() {
            let sequence = self
                .codex_peer_delivery_sequences
                .entry(pane_id)
                .or_default();
            *sequence = sequence.saturating_add(1);
            Some(*sequence)
        } else {
            self.pending_codex_peer_messages
                .get(&pane_id)
                .and_then(|queue| queue.front())
                .and_then(PendingCodexPeerDelivery::delivery_sequence)
        };
        let queue = self.pending_codex_peer_messages.entry(pane_id).or_default();
        if queue.is_empty() {
            queue.push_back(PendingCodexPeerDelivery::Draft {
                message,
                retries_remaining,
                stalled_since: Instant::now(),
                delivery_sequence,
            });
        }
        let queue_entries_total = queue.len();
        if let Some(path) = debug_path.as_deref() {
            log_codex_peer_decision(
                path,
                CodexPeerDecision {
                    pane_id,
                    delivery_sequence,
                    now: Instant::now(),
                    ready_at: None,
                    expires_at: None,
                    expected_composer: expected_composer.as_deref(),
                    expected_composer_raw: expected_composer_raw.as_deref(),
                    screen: screen.as_ref(),
                    composer_matches: None,
                    retries_remaining: Some(retries_remaining),
                    queue_entries_total,
                    other_queue_entries: queue_entries_total.saturating_sub(1),
                    action: if queue_was_empty {
                        "nudge_enqueued_draft"
                    } else {
                        "nudge_arrival_queue_occupied"
                    },
                },
            );
        }
    }

    fn codex_peer_debug_screen_snapshot(
        &self,
        pane_id: usize,
        capture_debug: bool,
    ) -> Option<CodexPeerScreenSnapshot> {
        if !capture_debug {
            return None;
        }
        let (ws_index, resolved_pane_id) =
            self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id))?;
        let registered_codex = self.peer_client_kinds.get(&pane_id) == Some(&PeerClientKind::Codex);
        let pane = self.workspaces[ws_index].panes.get(&resolved_pane_id)?;
        Self::codex_peer_screen_snapshot(registered_codex, pane, true)
    }

    fn show_codex_peer_notification(
        &mut self,
        pane_id: usize,
        message: PendingCodexPeerMessage,
        retries_remaining: Option<u8>,
    ) {
        self.pending_codex_peer_messages.remove(&pane_id);
        match self.codex_peer_notification.as_mut() {
            Some(notification) if notification.target_pane == pane_id => {
                notification.register_message(message, retries_remaining);
            }
            _ => {
                self.codex_peer_notification = Some(CodexPeerNotificationState {
                    target_pane: pane_id,
                    message,
                    pending_count: 1,
                    retries_remaining,
                });
            }
        }
        self.dirty = true;
    }

    fn route_focused_codex_peer_message(
        &mut self,
        pane_id: usize,
        message: PendingCodexPeerMessage,
    ) -> std::result::Result<bool, ipc::CodedError> {
        let registered_codex = self.peer_client_kinds.get(&pane_id) == Some(&PeerClientKind::Codex);
        let has_draft = self
            .ws()
            .panes
            .get(&pane_id)
            .and_then(codex_composer_has_draft)
            .unwrap_or(false);
        if has_draft {
            self.show_codex_peer_notification(pane_id, message, Some(CODEX_PEER_NUDGE_MAX_RETRIES));
            return Ok(true);
        }
        let ready = self
            .ws()
            .panes
            .get(&pane_id)
            .is_some_and(|pane| Self::codex_peer_delivery_ready(registered_codex, pane));
        if !ready {
            self.push_pending_codex_peer_nudge(pane_id, message);
            return Ok(false);
        }
        let debug_path = codex_peer_debug_log_path();
        let screen = self.codex_peer_debug_screen_snapshot(pane_id, debug_path.is_some());
        let payload_text = format_codex_peer_message(&message);
        let payload = crate::mcp_peer::build_send_keys_payload(&payload_text, None, false)
            .expect("codex peer draft payload");
        let pane =
            self.ws_mut().panes.get_mut(&pane_id).ok_or_else(|| {
                ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished")
            })?;
        write_input_to_pane(pane, payload.as_bytes(), false)?;
        let delivery_sequence = debug_path.as_ref().map(|_| {
            let sequence = self
                .codex_peer_delivery_sequences
                .entry(pane_id)
                .or_default();
            *sequence = sequence.saturating_add(1);
            *sequence
        });
        let ready_at = Instant::now() + CODEX_PEER_NUDGE_COMMIT_DELAY;
        let expected_composer = normalize_codex_composer_expected(&payload_text);
        let queue = self.pending_codex_peer_messages.entry(pane_id).or_default();
        queue.clear();
        queue.push_back(PendingCodexPeerDelivery::SubmitAt {
            ready_at,
            expected_composer: expected_composer.clone(),
            expected_composer_raw: debug_path.as_ref().map(|_| payload_text.clone()),
            delivery_sequence,
        });
        let queue_entries_total = queue.len();
        if let Some(path) = debug_path.as_deref() {
            log_codex_peer_decision(
                path,
                CodexPeerDecision {
                    pane_id,
                    delivery_sequence,
                    now: Instant::now(),
                    ready_at: Some(ready_at),
                    expires_at: None,
                    expected_composer: Some(&expected_composer),
                    expected_composer_raw: Some(&payload_text),
                    screen: screen.as_ref(),
                    composer_matches: None,
                    retries_remaining: Some(CODEX_PEER_NUDGE_MAX_RETRIES),
                    queue_entries_total,
                    other_queue_entries: queue_entries_total.saturating_sub(1),
                    action: "idle_draft_write_succeeded_submit_at_created",
                },
            );
        }
        self.codex_peer_notification = None;
        self.dirty = true;
        Ok(false)
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
        self.restore_codex_peer_notification(notification);
        self.dirty = true;
    }

    fn restore_codex_peer_notification(&mut self, notification: CodexPeerNotificationState) {
        match notification.retries_remaining {
            Some(retries_remaining) => self.push_pending_codex_peer_nudge_with_retries(
                notification.target_pane,
                notification.message,
                retries_remaining,
            ),
            None => {
                let queue = self
                    .pending_codex_peer_messages
                    .entry(notification.target_pane)
                    .or_default();
                if queue.is_empty() {
                    queue.push_back(PendingCodexPeerDelivery::AwaitFocus {
                        message: notification.message,
                        retries_remaining: 0,
                        delivery_sequence: None,
                    });
                }
            }
        }
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
            self.restore_codex_peer_notification(notification.clone());
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
        // Accept is deliberately unavailable while a draft is present. Writing
        // the nudge would append it to the draft, and the delayed Enter below
        // would silently submit both as one user turn. Keep the overlay visible
        // so the user can send, save, or clear the draft before accepting.
        let composer_is_empty = self
            .ws()
            .panes
            .get(&notification.target_pane)
            .and_then(codex_composer_has_draft)
            == Some(false);
        if !composer_is_empty {
            return Ok(true);
        }
        let payload_text = format_codex_peer_message(&notification.message);
        let debug_path = codex_peer_debug_log_path();
        let screen =
            self.codex_peer_debug_screen_snapshot(notification.target_pane, debug_path.is_some());
        let payload = crate::mcp_peer::build_send_keys_payload(&payload_text, None, false)
            .expect("codex peer notification payload");
        let pane = self
            .ws_mut()
            .panes
            .get_mut(&notification.target_pane)
            .ok_or_else(|| ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished"))?;
        write_input_to_pane(pane, payload.as_bytes(), false)?;
        let delivery_sequence = debug_path.as_ref().map(|_| {
            let sequence = self
                .codex_peer_delivery_sequences
                .entry(notification.target_pane)
                .or_default();
            *sequence = sequence.saturating_add(1);
            *sequence
        });
        let ready_at = Instant::now() + CODEX_PEER_NUDGE_COMMIT_DELAY;
        let expected_composer = normalize_codex_composer_expected(&payload_text);
        let queue = self
            .pending_codex_peer_messages
            .entry(notification.target_pane)
            .or_default();
        queue.clear();
        queue.push_back(PendingCodexPeerDelivery::SubmitAt {
            ready_at,
            expected_composer: expected_composer.clone(),
            expected_composer_raw: debug_path.as_ref().map(|_| payload_text.clone()),
            delivery_sequence,
        });
        let queue_entries_total = queue.len();
        if let Some(path) = debug_path.as_deref() {
            log_codex_peer_decision(
                path,
                CodexPeerDecision {
                    pane_id: notification.target_pane,
                    delivery_sequence,
                    now: Instant::now(),
                    ready_at: Some(ready_at),
                    expires_at: None,
                    expected_composer: Some(&expected_composer),
                    expected_composer_raw: Some(&payload_text),
                    screen: screen.as_ref(),
                    composer_matches: None,
                    retries_remaining: notification.retries_remaining,
                    queue_entries_total,
                    other_queue_entries: queue_entries_total.saturating_sub(1),
                    action: "notification_draft_write_succeeded_submit_at_created",
                },
            );
        }
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
        let lock_started = super::frame_diagnostics::lock_wait_started();
        let Ok(parser) = pane.parser.lock() else {
            return false;
        };
        super::frame_diagnostics::record_lock_wait(pane.id, lock_started);
        analyze_codex_peer_screen(parser.screen(), false).ready_for_nudge
    }

    fn codex_peer_screen_snapshot(
        registered_codex: bool,
        pane: &Pane,
        capture_debug: bool,
    ) -> Option<CodexPeerScreenSnapshot> {
        if !registered_codex && !pane.is_codex_running() {
            return None;
        }
        let lock_started = super::frame_diagnostics::lock_wait_started();
        let Ok(parser) = pane.parser.lock() else {
            return None;
        };
        super::frame_diagnostics::record_lock_wait(pane.id, lock_started);
        Some(analyze_codex_peer_screen(parser.screen(), capture_debug))
    }

    pub(crate) fn flush_pending_codex_peer_messages(&mut self) {
        self.materialize_unfocused_codex_peer_notification();
        let now = Instant::now();
        let codex_peer_debug_log_path = codex_peer_debug_log_path();
        let codex_peer_debug_observations = &mut self.codex_peer_debug_observations;
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
                    let screen = Self::codex_peer_screen_snapshot(
                        registered_codex,
                        pane,
                        codex_peer_debug_log_path.is_some(),
                    );
                    match delivery {
                        PendingCodexPeerDelivery::Draft {
                            message,
                            retries_remaining,
                            stalled_since,
                            delivery_sequence,
                        } => {
                            let queue_entries_total = queue.len();
                            let mut log_decision = |action, only_if_changed| {
                                let Some(path) = codex_peer_debug_log_path.as_deref() else {
                                    return;
                                };
                                let decision = CodexPeerDecision {
                                    pane_id,
                                    delivery_sequence,
                                    now,
                                    ready_at: None,
                                    expires_at: None,
                                    expected_composer: None,
                                    expected_composer_raw: None,
                                    screen: screen.as_ref(),
                                    composer_matches: None,
                                    retries_remaining: Some(retries_remaining),
                                    queue_entries_total,
                                    other_queue_entries: queue_entries_total.saturating_sub(1),
                                    action,
                                };
                                if only_if_changed {
                                    log_codex_peer_decision_if_changed(
                                        path,
                                        codex_peer_debug_observations,
                                        decision,
                                    );
                                } else {
                                    log_codex_peer_decision(path, decision);
                                }
                            };
                            if screen.as_ref().and_then(|state| state.has_draft) == Some(true) {
                                if pane_is_focused {
                                    log_decision("draft_has_draft_focused_notification", false);
                                    queue.pop_front();
                                    focused_notifications.push((
                                        pane_id,
                                        message,
                                        Some(retries_remaining),
                                    ));
                                    self.dirty = true;
                                } else {
                                    log_decision("draft_waiting_has_draft", true);
                                }
                                continue;
                            }
                            let payload_text = format_codex_peer_message(&message);
                            if screen.as_ref().is_some_and(|state| state.can_queue_message)
                                && screen.as_ref().and_then(|state| state.has_draft) != Some(true)
                            {
                                let payload = crate::mcp_peer::build_send_keys_payload(
                                    &payload_text,
                                    None,
                                    false,
                                )
                                .expect("codex peer draft payload");
                                if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                    let ready_at = now + CODEX_PEER_NUDGE_COMMIT_DELAY;
                                    let expires_at = now + CODEX_PEER_NUDGE_COMMIT_TIMEOUT;
                                    let expected_composer =
                                        normalize_codex_composer_expected(&payload_text);
                                    if let Some(path) = codex_peer_debug_log_path.as_deref() {
                                        log_codex_peer_decision(
                                            path,
                                            CodexPeerDecision {
                                                pane_id,
                                                delivery_sequence,
                                                now,
                                                ready_at: Some(ready_at),
                                                expires_at: Some(expires_at),
                                                expected_composer: Some(&expected_composer),
                                                expected_composer_raw: Some(&payload_text),
                                                screen: screen.as_ref(),
                                                composer_matches: None,
                                                retries_remaining: Some(retries_remaining),
                                                queue_entries_total,
                                                other_queue_entries: queue_entries_total
                                                    .saturating_sub(1),
                                                action: "draft_write_succeeded_queue_at_created",
                                            },
                                        );
                                    }
                                    queue.pop_front();
                                    queue.push_front(PendingCodexPeerDelivery::QueueAt {
                                        // Keep the commit key in a later PTY write so Codex
                                        // does not interpret text plus Tab/Enter as a paste.
                                        ready_at,
                                        expires_at,
                                        message,
                                        expected_composer,
                                        expected_composer_raw: codex_peer_debug_log_path
                                            .as_ref()
                                            .map(|_| payload_text),
                                        delivery_sequence,
                                        retries_remaining,
                                    });
                                    self.dirty = true;
                                    continue;
                                } else {
                                    log_decision("draft_queue_write_failed", true);
                                }
                            }
                            if screen.as_ref().is_some_and(|state| state.ready_for_nudge) {
                                let payload = crate::mcp_peer::build_send_keys_payload(
                                    &payload_text,
                                    None,
                                    false,
                                )
                                .expect("codex peer draft payload");
                                if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                    let ready_at = now + CODEX_PEER_NUDGE_COMMIT_DELAY;
                                    let expected_composer =
                                        normalize_codex_composer_expected(&payload_text);
                                    if let Some(path) = codex_peer_debug_log_path.as_deref() {
                                        log_codex_peer_decision(
                                            path,
                                            CodexPeerDecision {
                                                pane_id,
                                                delivery_sequence,
                                                now,
                                                ready_at: Some(ready_at),
                                                expires_at: None,
                                                expected_composer: Some(&expected_composer),
                                                expected_composer_raw: Some(&payload_text),
                                                screen: screen.as_ref(),
                                                composer_matches: None,
                                                retries_remaining: Some(retries_remaining),
                                                queue_entries_total,
                                                other_queue_entries: queue_entries_total
                                                    .saturating_sub(1),
                                                action: "draft_write_succeeded_submit_at_created",
                                            },
                                        );
                                    }
                                    queue.pop_front();
                                    queue.push_front(PendingCodexPeerDelivery::SubmitAt {
                                        ready_at,
                                        expected_composer,
                                        expected_composer_raw: codex_peer_debug_log_path
                                            .as_ref()
                                            .map(|_| payload_text),
                                        delivery_sequence,
                                    });
                                    self.dirty = true;
                                    continue;
                                } else {
                                    log_decision("draft_submit_write_failed", true);
                                }
                            }
                            if now.saturating_duration_since(stalled_since)
                                >= CODEX_PEER_DRAFT_STALL_TIMEOUT
                            {
                                log_decision(
                                    if pane_is_focused {
                                        "draft_stalled_to_focused_notification"
                                    } else {
                                        "draft_stalled_to_await_focus"
                                    },
                                    false,
                                );
                                queue.pop_front();
                                if pane_is_focused {
                                    focused_notifications.push((
                                        pane_id,
                                        message,
                                        Some(retries_remaining),
                                    ));
                                } else {
                                    queue.push_front(PendingCodexPeerDelivery::AwaitFocus {
                                        message,
                                        retries_remaining,
                                        delivery_sequence,
                                    });
                                }
                                self.dirty = true;
                            } else {
                                log_decision("draft_waiting_no_safe_input_path", true);
                            }
                        }
                        PendingCodexPeerDelivery::SubmitAt {
                            ready_at,
                            expected_composer,
                            expected_composer_raw,
                            delivery_sequence,
                        } => {
                            let queue_entries_total = queue.len();
                            let composer_matches =
                                screen.as_ref().and_then(|state| state.composer.as_ref())
                                    == Some(&expected_composer);
                            let mut log_decision = |action, only_if_changed| {
                                let Some(path) = codex_peer_debug_log_path.as_deref() else {
                                    return;
                                };
                                let decision = CodexPeerDecision {
                                    pane_id,
                                    delivery_sequence,
                                    now,
                                    ready_at: Some(ready_at),
                                    expires_at: None,
                                    expected_composer: Some(&expected_composer),
                                    expected_composer_raw: expected_composer_raw.as_deref(),
                                    screen: screen.as_ref(),
                                    composer_matches: Some(composer_matches),
                                    retries_remaining: None,
                                    queue_entries_total,
                                    other_queue_entries: queue_entries_total.saturating_sub(1),
                                    action,
                                };
                                if only_if_changed {
                                    log_codex_peer_decision_if_changed(
                                        path,
                                        codex_peer_debug_observations,
                                        decision,
                                    );
                                } else {
                                    log_codex_peer_decision(path, decision);
                                }
                            };
                            if now < ready_at {
                                log_decision("submit_at_waiting_ready_at", true);
                                continue;
                            }
                            if !composer_matches {
                                log_decision("submit_at_composer_mismatch", true);
                                continue;
                            }
                            let payload = crate::mcp_peer::build_send_keys_payload("", None, true)
                                .expect("codex peer submit payload");
                            if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                log_decision("submit_at_enter_pressed", false);
                                queue.pop_front();
                                self.dirty = true;
                            } else {
                                log_decision("submit_at_enter_write_failed", true);
                            }
                        }
                        PendingCodexPeerDelivery::QueueAt {
                            ready_at,
                            expires_at,
                            message,
                            expected_composer,
                            expected_composer_raw,
                            delivery_sequence,
                            retries_remaining,
                        } => {
                            let composer_matches =
                                screen.as_ref().and_then(|state| state.composer.as_ref())
                                    == Some(&expected_composer);
                            let queue_entries_total = queue.len();
                            let mut log_decision = |action, only_if_changed| {
                                if let Some(path) = codex_peer_debug_log_path.as_deref() {
                                    let decision = CodexPeerDecision {
                                        pane_id,
                                        delivery_sequence,
                                        now,
                                        ready_at: Some(ready_at),
                                        expires_at: Some(expires_at),
                                        expected_composer: Some(&expected_composer),
                                        expected_composer_raw: expected_composer_raw.as_deref(),
                                        screen: screen.as_ref(),
                                        composer_matches: Some(composer_matches),
                                        retries_remaining: Some(retries_remaining),
                                        queue_entries_total,
                                        other_queue_entries: queue_entries_total.saturating_sub(1),
                                        action,
                                    };
                                    if only_if_changed {
                                        log_codex_peer_decision_if_changed(
                                            path,
                                            codex_peer_debug_observations,
                                            decision,
                                        );
                                    } else {
                                        log_codex_peer_decision(path, decision);
                                    }
                                }
                            };
                            if now >= expires_at {
                                if pane_is_focused {
                                    if composer_matches {
                                        let _ = write_input_to_pane(pane, b"\x15", false);
                                        log_decision(
                                            "focused_queue_expired_cleared_and_notified",
                                            false,
                                        );
                                    } else {
                                        log_decision(
                                            "focused_queue_expired_notified_without_clear",
                                            false,
                                        );
                                    }
                                    queue.pop_front();
                                    focused_notifications.push((
                                        pane_id,
                                        message,
                                        Some(retries_remaining),
                                    ));
                                    self.dirty = true;
                                    continue;
                                }
                                if composer_matches {
                                    let _ = write_input_to_pane(pane, b"\x15", false);
                                    log_decision("expired_cleared_and_requeued", false);
                                } else {
                                    log_decision("expired_discarded_without_ctrl_u", false);
                                }
                                queue.pop_front();
                                if retries_remaining > 0 {
                                    queue.push_front(PendingCodexPeerDelivery::Draft {
                                        message,
                                        retries_remaining: retries_remaining - 1,
                                        stalled_since: now,
                                        delivery_sequence,
                                    });
                                } else {
                                    queue.push_front(PendingCodexPeerDelivery::AwaitFocus {
                                        message,
                                        retries_remaining: 0,
                                        delivery_sequence,
                                    });
                                }
                                self.dirty = true;
                                continue;
                            }
                            if now < ready_at {
                                log_decision("continued_waiting_ready_at", true);
                                continue;
                            }
                            if !composer_matches {
                                log_decision("continued_composer_mismatch", true);
                                continue;
                            }
                            let commit = if screen
                                .as_ref()
                                .is_some_and(|state| state.busy_queue_available)
                            {
                                Some((b"\t".as_slice(), "tab_pressed", "tab_write_failed"))
                            } else if screen
                                .as_ref()
                                .is_some_and(|state| state.can_submit_injected_message)
                                && !pane_is_focused
                            {
                                Some((b"\r".as_slice(), "enter_pressed", "enter_write_failed"))
                            } else if screen
                                .as_ref()
                                .is_some_and(|state| state.can_submit_injected_message)
                                && pane_is_focused
                            {
                                let _ = write_input_to_pane(pane, b"\x15", false);
                                log_decision(
                                    "focused_queue_enter_blocked_cleared_and_notified",
                                    false,
                                );
                                queue.pop_front();
                                focused_notifications.push((
                                    pane_id,
                                    message,
                                    Some(retries_remaining),
                                ));
                                self.dirty = true;
                                continue;
                            } else {
                                log_decision("continued_no_commit_key_available", true);
                                None
                            };
                            let Some((payload, success_action, failure_action)) = commit else {
                                continue;
                            };
                            if write_input_to_pane(pane, payload, false).is_ok() {
                                log_decision(success_action, false);
                                queue.pop_front();
                                self.dirty = true;
                            } else {
                                log_decision(failure_action, true);
                            }
                        }
                        PendingCodexPeerDelivery::AwaitFocus {
                            message,
                            retries_remaining,
                            delivery_sequence,
                        } => {
                            let queue_entries_total = queue.len();
                            let mut log_decision = |action, only_if_changed| {
                                let Some(path) = codex_peer_debug_log_path.as_deref() else {
                                    return;
                                };
                                let decision = CodexPeerDecision {
                                    pane_id,
                                    delivery_sequence,
                                    now,
                                    ready_at: None,
                                    expires_at: None,
                                    expected_composer: None,
                                    expected_composer_raw: None,
                                    screen: screen.as_ref(),
                                    composer_matches: None,
                                    retries_remaining: Some(retries_remaining),
                                    queue_entries_total,
                                    other_queue_entries: queue_entries_total.saturating_sub(1),
                                    action,
                                };
                                if only_if_changed {
                                    log_codex_peer_decision_if_changed(
                                        path,
                                        codex_peer_debug_observations,
                                        decision,
                                    );
                                } else {
                                    log_codex_peer_decision(path, decision);
                                }
                            };
                            if pane_is_focused {
                                log_decision("await_focus_focused_notification", false);
                                queue.pop_front();
                                focused_notifications.push((
                                    pane_id,
                                    message,
                                    (retries_remaining > 0).then_some(retries_remaining),
                                ));
                                self.dirty = true;
                            } else if screen.as_ref().is_some_and(|state| {
                                state.ready_for_nudge && state.has_draft == Some(false)
                            }) {
                                log_decision("await_focus_ready_restarted_draft", false);
                                queue.pop_front();
                                queue.push_front(PendingCodexPeerDelivery::Draft {
                                    message,
                                    retries_remaining,
                                    stalled_since: now,
                                    delivery_sequence,
                                });
                                self.dirty = true;
                            } else {
                                log_decision("await_focus_waiting", true);
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
            self.codex_peer_debug_observations.remove(&pane_id);
        }
        for (pane_id, message, retries_remaining) in focused_notifications {
            self.show_codex_peer_notification(pane_id, message, retries_remaining);
        }
    }
}

#[cfg(test)]
mod debug_logging_tests {
    use super::*;

    #[test]
    fn inherited_debug_log_path_is_disabled_by_default() {
        let env_read = std::cell::Cell::new(false);
        let path = resolve_codex_peer_debug_log_path(|| {
            env_read.set(true);
            Some(std::ffi::OsString::from("inherited-debug-log.jsonl"))
        });

        assert_eq!(path, None);
        assert!(!env_read.get());
    }

    #[test]
    fn production_debug_log_path_reads_the_environment() {
        let expected = std::ffi::OsString::from("production-debug-log.jsonl");
        let env_read = std::cell::Cell::new(false);
        set_codex_peer_debug_log_path_test_override(None);

        let path = resolve_codex_peer_debug_log_path(|| {
            env_read.set(true);
            Some(expected.clone())
        });
        set_codex_peer_debug_log_path_test_override(Some(None));

        assert_eq!(path, Some(expected));
        assert!(env_read.get());
    }

    #[test]
    fn injected_debug_log_path_writes_from_kind_update_call_site() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "renga-codex-peer-kind-update-{}-{unique}.jsonl",
            std::process::id()
        ));
        set_codex_peer_debug_log_path_test_override(Some(Some(path.as_os_str().to_owned())));

        log_codex_peer_kind_update(
            17,
            Some(PeerClientKind::Claude),
            PeerClientKind::Codex,
            "test",
        );
        set_codex_peer_debug_log_path_test_override(Some(None));

        let contents = std::fs::read_to_string(&path).expect("debug JSONL");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: serde_json::Value = serde_json::from_str(lines[0]).expect("one JSON object");
        assert_eq!(record["action"], "client_kind_updated");
        assert_eq!(record["pane_id"], 17);
        std::fs::remove_file(path).expect("remove debug JSONL");
    }

    #[test]
    fn production_wiring_reads_renga_debug_codex_peer_log() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "renga-codex-peer-production-wiring-{}-{unique}.jsonl",
            std::process::id()
        ));

        // This process-wide mutation intentionally verifies the literal env
        // wiring. The resolver has no OnceLock, and every other test keeps its
        // test override disabled, so the temporary path cannot leak records to
        // an inherited live trace even when tests run in parallel.
        set_codex_peer_debug_log_path_test_override(None);
        let previous = std::env::var_os("RENGA_DEBUG_CODEX_PEER_LOG");
        std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", &path);
        let resolved = codex_peer_debug_log_path();
        log_codex_peer_kind_update(23, None, PeerClientKind::Codex, "production_wiring_test");
        match previous {
            Some(value) => std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", value),
            None => std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG"),
        }
        set_codex_peer_debug_log_path_test_override(Some(None));

        assert_eq!(resolved.as_deref(), Some(path.as_os_str()));
        let contents = std::fs::read_to_string(&path).expect("debug JSONL");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: serde_json::Value = serde_json::from_str(lines[0]).expect("one JSON object");
        assert_eq!(record["action"], "client_kind_updated");
        assert_eq!(record["pane_id"], 23);
        std::fs::remove_file(path).expect("remove debug JSONL");
    }

    #[test]
    fn debug_capture_preserves_composer_and_writes_single_jsonl_record_per_state() {
        let mut parser = vt100::Parser::new(20, 80, 0);
        parser.process(
            b"\x1b[?25h\x1b[2J\x1b[3;1H\xE2\x97\xA6 Working (12s \xE2\x80\xA2 esc to interrupt)\x1b[6;1H\xE2\x80\xBA peer nudge\x1b[10;1H  tab to queue message  51% context left\x1b[6;14H",
        );
        let normal = analyze_codex_peer_screen(parser.screen(), false);
        let debug = analyze_codex_peer_screen(parser.screen(), true);
        assert_eq!(normal.composer, debug.composer);

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "renga-codex-peer-debug-{}-{unique}.jsonl",
            std::process::id()
        ));
        let mut observations = HashMap::new();
        for _ in 0..2 {
            log_codex_peer_decision_if_changed(
                path.as_os_str(),
                &mut observations,
                CodexPeerDecision {
                    pane_id: 7,
                    delivery_sequence: Some(1),
                    now: Instant::now(),
                    ready_at: None,
                    expires_at: None,
                    expected_composer: Some("peer nudge"),
                    expected_composer_raw: Some("peer nudge"),
                    screen: Some(&debug),
                    composer_matches: Some(true),
                    retries_remaining: Some(1),
                    queue_entries_total: 1,
                    other_queue_entries: 0,
                    action: "continued_composer_match",
                },
            );
        }
        let contents = std::fs::read_to_string(&path).expect("debug JSONL");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: serde_json::Value = serde_json::from_str(lines[0]).expect("one JSON object");
        assert_eq!(record["action"], "continued_composer_match");
        assert_eq!(record["screen_composer"], "peernudge");
        std::fs::remove_file(path).expect("remove debug JSONL");
    }

    #[test]
    fn debug_capture_preserves_four_line_wrapped_composer() {
        let mut parser = vt100::Parser::new(16, 56, 0);
        let message = "Peer request from id=1 name=shogun kind=claude. Run check_messages now. Treat each returned message as a direct coworker request: do the requested work, and use send_message only when a reply or status update is needed.";
        let fixture = format!(
            "\x1b[?25h\x1b[2J\x1b[H\u{203a} {message}\r\n\r\n  gpt-5.6-sol medium \u{b7} cwd\x1b[4;48H"
        );
        parser.process(fixture.as_bytes());

        let normal = analyze_codex_peer_screen(parser.screen(), false);
        let debug = analyze_codex_peer_screen(parser.screen(), true);
        let raw = debug
            .debug
            .as_ref()
            .and_then(|snapshot| snapshot.composer_raw.as_deref())
            .expect("debug composer");
        assert_eq!(raw.lines().count(), 4, "fixture must exercise row joins");
        assert_eq!(normal.composer, debug.composer);
        assert_eq!(
            normal.composer,
            Some(normalize_codex_composer_expected(message))
        );
    }
}
