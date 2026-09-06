//! `renga mcp-peer` — the stdio MCP server Claude Code spawns per pane.
//!
//! Stage 3 of issue #97: the real implementation that replaces
//! `src/bin/renga-mcp-peer-spike.rs`. Where the spike looped messages
//! back to the same Claude, this module routes them through renga's
//! existing IPC server so a message sent from pane A shows up in pane
//! B's context as a `<channel source="renga-peers">` tag — provided
//! both panes live in the same renga tab.
//!
//! # Lifecycle
//!
//! 1. Claude Code spawns `renga mcp-peer` as a stdio subprocess. The
//!    PTY env published by renga (`RENGA_PANE_ID`, `RENGA_SOCKET`,
//!    `RENGA_TOKEN`) is inherited all the way down.
//! 2. [`run`] negotiates the MCP `initialize` handshake, declares the
//!    `claude/channel` experimental capability, and spawns a background
//!    thread that subscribes to renga's event bus before registering the
//!    pane as ready for peer delivery.
//! 3. Inbound `Request::PeerSend` deliveries land on the event bus as
//!    [`crate::ipc::Event::PeerInbox`]. The background thread filters
//!    on `target_pane == our RENGA_PANE_ID` and pushes a
//!    `notifications/claude/channel` frame to stdout — the only thing
//!    that makes peer messages show up as a channel tag instead of an
//!    ordinary tool result.
//!
//! # Outside-renga fallback
//!
//! If `RENGA_PANE_ID` is absent (Claude was launched from a terminal
//! renga didn't spawn), the module still handshakes and advertises the
//! tools — they just return empty/no-op results. This keeps the stdio
//! MCP installed globally in `~/.claude/mcp_servers.json` from erroring
//! out every time Claude starts outside renga.

pub mod install;
mod parent_watch;

use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::app::CLAUDE_PEER_LAUNCH_CMD;
use crate::ipc::endpoint::{endpoint_from_env, EndpointName, ENV_SOCKET, ENV_TOKEN};
use crate::ipc::{
    self, client, Direction, PaneInfo, PaneRef, PeerClientKind, PeerInfo, Request, Response,
};

const SERVER_NAME: &str = "renga-peers";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const ENV_PANE_ID: &str = "RENGA_PANE_ID";
pub(crate) const ENV_CLIENT_KIND: &str = "RENGA_PEER_CLIENT_KIND";
const ENV_DEBUG_CODEX_PEER_LOG: &str = "RENGA_DEBUG_CODEX_PEER_LOG";
const PUSH_READY_DELAY: Duration = Duration::from_millis(1500);
static PEER_DEBUG_RECORD_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);
static PEER_DEBUG_WRITE_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn log_stderr(msg: &str) {
    eprintln!("[renga-mcp-peer] {msg}");
}

/// Entry point called by `renga mcp-peer`. Blocks on stdin until EOF
/// or an unrecoverable error — with a parent-process watchdog as the
/// authoritative backstop, because stdin EOF is not guaranteed to
/// arrive on Windows when the pipe's write end leaked into sibling
/// processes via handle inheritance (renga-9fs).
pub fn run() -> Result<()> {
    log_stderr(&format!("starting {SERVER_NAME} v{SERVER_VERSION}"));

    parent_watch::spawn(|| {
        log_stderr("parent process exited; shutting down");
        std::process::exit(0);
    });

    let ctx = PeerCtx::load();
    match &ctx.mode {
        Mode::Connected { pane_id, .. } => {
            log_stderr(&format!(
                "connected mode: pane_id={pane_id}, client_kind={:?}",
                ctx.client_kind
            ));
            // Kind metadata is useful even if the event subscription is
            // temporarily unavailable. Delivery readiness is published
            // separately after the subscribe acknowledgement.
            register_client_kind(&ctx);
            spawn_inbox_subscriber(ctx.clone());
        }
        Mode::Detached { reason } => {
            log_stderr(&format!("detached mode: {reason}"));
        }
    }

    stdio_loop(&ctx)
}

fn register_client_kind(ctx: &PeerCtx) {
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return;
    };
    match client::send_request(
        endpoint,
        &Request::PeerRegisterClient {
            pane_id: *pane_id,
            kind: ctx.client_kind,
        },
    ) {
        Ok(Response::Ok { .. }) => {}
        Ok(other) => log_stderr(&format!("peer kind registration returned: {other:?}")),
        Err(e) => log_stderr(&format!("peer kind registration failed: {e}")),
    }
}

fn set_client_ready(ctx: &PeerCtx, ready: bool) {
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return;
    };
    let initialized_age_ms = ctx
        .push
        .lock()
        .unwrap_or_else(|state| state.into_inner())
        .initialized_at
        .map(|initialized_at| initialized_at.elapsed().as_millis());
    let request = Request::PeerSetReady {
        pane_id: *pane_id,
        kind: ctx.client_kind,
        ready,
    };
    #[cfg(test)]
    let result = if let Some(sink) = &ctx.request_sink {
        sink.lock()
            .unwrap_or_else(|requests| requests.into_inner())
            .push(request);
        Ok(ctx
            .request_sink_response
            .clone()
            .unwrap_or_else(Response::ok_unit))
    } else {
        client::send_request(endpoint, &request)
    };
    #[cfg(not(test))]
    let result = client::send_request(endpoint, &request);
    match &result {
        Ok(Response::Ok { .. }) => {}
        Ok(other) => log_stderr(&format!("peer readiness update returned: {other:?}")),
        Err(e) => log_stderr(&format!("peer readiness update failed: {e}")),
    }
    if let Some(path) = ctx.debug_log_path.as_deref() {
        let (ok, error) = match &result {
            Ok(Response::Ok { .. }) => (true, None),
            Ok(other) => (false, Some(format!("unexpected response: {other:?}"))),
            Err(error) => (false, Some(error.to_string())),
        };
        append_peer_debug_record(
            path,
            Some(*pane_id),
            json!({
                "action": "peer_set_ready_sent",
                "client_kind": kind_label(ctx.client_kind),
                "ready": ready,
                "initialized_age_ms": initialized_age_ms,
                "ok": ok,
                "error": error,
            }),
        );
    }
}

fn acknowledge_peer_inbox(ctx: &PeerCtx, delivery_id: u64) {
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return;
    };
    let request = Request::PeerInboxAck {
        pane_id: *pane_id,
        delivery_id,
    };
    #[cfg(test)]
    let result = if let Some(sink) = &ctx.request_sink {
        sink.lock()
            .unwrap_or_else(|requests| requests.into_inner())
            .push(request);
        Ok(ctx
            .request_sink_response
            .clone()
            .unwrap_or_else(Response::ok_unit))
    } else {
        client::send_request(endpoint, &request)
    };
    #[cfg(not(test))]
    let result = client::send_request(endpoint, &request);
    match &result {
        Ok(Response::Ok { .. }) => {}
        Ok(other) => log_stderr(&format!("peer inbox receipt returned: {other:?}")),
        Err(e) => log_stderr(&format!("peer inbox receipt failed: {e}")),
    }
    let Some(path) = ctx.debug_log_path.as_deref() else {
        return;
    };
    let (ok, error) = match result {
        Ok(Response::Ok { .. }) => (true, None),
        Ok(other) => (false, Some(format!("unexpected response: {other:?}"))),
        Err(error) => (false, Some(error.to_string())),
    };
    log_peer_inbox_ack_sent(path, *pane_id, delivery_id, ok, error.as_deref());
}

struct PeerInboxAckQueue {
    sender: mpsc::Sender<PeerInboxQueuedRequest>,
    depth: Arc<AtomicUsize>,
    in_flight: Arc<AtomicBool>,
    cancel_drain: Arc<AtomicBool>,
    ctx: PeerCtx,
    handle: thread::JoinHandle<()>,
    done: mpsc::Receiver<()>,
}

impl PeerInboxAckQueue {
    fn enqueue(&self, delivery_id: u64) {
        self.enqueue_request(PeerInboxQueuedRequest::Ack(delivery_id));
    }

    fn enqueue_request(&self, request: PeerInboxQueuedRequest) {
        let depth = self.depth.fetch_add(1, Ordering::AcqRel) + 1;
        let delivery_id = request.delivery_id();
        if let Some(path) = self.ctx.debug_log_path.as_deref() {
            let pane_id = match &self.ctx.mode {
                Mode::Connected { pane_id, .. } => Some(*pane_id),
                Mode::Detached { .. } => None,
            };
            append_peer_debug_record(
                path,
                pane_id,
                json!({
                    "action": request.queue_action(),
                    "delivery_id": delivery_id,
                    "depth": depth,
                }),
            );
        }
        if self.sender.send(request).is_err() {
            self.depth.fetch_sub(1, Ordering::AcqRel);
            log_stderr(&format!(
                "peer inbox receipt queue closed before delivery {delivery_id} was accepted"
            ));
        }
    }

    fn finish(self) {
        self.finish_with_timeout(ipc::RESPONSE_TIMEOUT);
    }

    fn finish_with_timeout(self, timeout: Duration) {
        let PeerInboxAckQueue {
            sender,
            depth,
            in_flight,
            cancel_drain,
            ctx,
            handle,
            done,
        } = self;
        *ctx.peer_inbox_request_sender
            .lock()
            .unwrap_or_else(|slot| slot.into_inner()) = PeerInboxRequestRoute::Unavailable;
        drop(sender);
        match done.recv_timeout(timeout) {
            Ok(()) => {
                if handle.join().is_err() {
                    log_stderr("peer inbox receipt sender panicked while draining");
                }
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                log_stderr("peer inbox receipt sender stopped before reporting drain completion");
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        cancel_drain.store(true, Ordering::Release);
        let dropped_count = depth.load(Ordering::Acquire);
        let pending_count = dropped_count + usize::from(in_flight.load(Ordering::Acquire));
        if let Some(path) = ctx.debug_log_path.as_deref() {
            let pane_id = match &ctx.mode {
                Mode::Connected { pane_id, .. } => Some(*pane_id),
                Mode::Detached { .. } => None,
            };
            append_peer_debug_record(
                path,
                pane_id,
                json!({
                    "action": "peer_inbox_ack_drain_abandoned",
                    "pending_count": pending_count,
                    "dropped_count": dropped_count,
                }),
            );
        }
        log_stderr(&format!(
            "peer inbox receipt drain exceeded {:?}; dropping {dropped_count} queued receipt(s)",
            timeout
        ));
        drop(handle);
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.depth.load(Ordering::Acquire)
    }
}

enum PeerInboxAckSender {
    Async(PeerInboxAckQueue),
    Sync(PeerCtx),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerInboxQueuedRequest {
    Ack(u64),
    Consumed(u64),
}

#[derive(Clone)]
enum PeerInboxRequestRoute {
    Unavailable,
    Async {
        sender: mpsc::Sender<PeerInboxQueuedRequest>,
        depth: Arc<AtomicUsize>,
    },
    Sync,
}

impl PeerInboxQueuedRequest {
    fn delivery_id(self) -> u64 {
        match self {
            Self::Ack(id) | Self::Consumed(id) => id,
        }
    }

    fn queue_action(self) -> &'static str {
        match self {
            Self::Ack(_) => "peer_inbox_ack_queued",
            Self::Consumed(_) => "peer_inbox_consumed_queued",
        }
    }
}

impl PeerInboxAckSender {
    fn enqueue(&self, delivery_id: u64) {
        match self {
            Self::Async(queue) => queue.enqueue(delivery_id),
            Self::Sync(ctx) => acknowledge_peer_inbox(ctx, delivery_id),
        }
    }

    fn finish(self) {
        if let Self::Async(queue) = self {
            queue.finish();
        }
    }

    #[cfg(test)]
    fn finish_with_timeout(self, timeout: Duration) {
        if let Self::Async(queue) = self {
            queue.finish_with_timeout(timeout);
        }
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        match self {
            Self::Async(queue) => queue.depth(),
            Self::Sync(_) => 0,
        }
    }
}

fn spawn_peer_inbox_ack_sender(ctx: PeerCtx) -> PeerInboxAckSender {
    let (sender, receiver) = mpsc::channel();
    let (done_tx, done) = mpsc::channel();
    let depth = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicBool::new(false));
    let cancel_drain = Arc::new(AtomicBool::new(false));
    let worker_depth = depth.clone();
    let worker_in_flight = in_flight.clone();
    let worker_cancel_drain = cancel_drain.clone();
    let worker_ctx = ctx.clone();
    let spawn_result = thread::Builder::new()
        .name("renga-mcp-peer-ack".into())
        .spawn(move || {
            while let Ok(request) = receiver.recv() {
                worker_depth.fetch_sub(1, Ordering::AcqRel);
                worker_in_flight.store(true, Ordering::Release);
                match request {
                    PeerInboxQueuedRequest::Ack(delivery_id) => {
                        acknowledge_peer_inbox(&worker_ctx, delivery_id)
                    }
                    PeerInboxQueuedRequest::Consumed(delivery_id) => {
                        if notify_peer_inbox_consumed(&worker_ctx, delivery_id) {
                            mark_consumed_reported(&worker_ctx, delivery_id);
                        }
                    }
                }
                worker_in_flight.store(false, Ordering::Release);
                if worker_cancel_drain.load(Ordering::Acquire) {
                    let dropped = receiver.try_iter().count();
                    worker_depth.fetch_sub(dropped, Ordering::AcqRel);
                    break;
                }
            }
            let _ = done_tx.send(());
        });
    match spawn_result {
        Ok(handle) => {
            *ctx.peer_inbox_request_sender
                .lock()
                .unwrap_or_else(|slot| slot.into_inner()) = PeerInboxRequestRoute::Async {
                sender: sender.clone(),
                depth: depth.clone(),
            };
            PeerInboxAckSender::Async(PeerInboxAckQueue {
                sender,
                depth,
                in_flight,
                cancel_drain,
                ctx,
                handle,
                done,
            })
        }
        Err(error) => {
            log_stderr(&format!(
                "failed to spawn peer inbox receipt sender: {error}; using synchronous receipts"
            ));
            *ctx.peer_inbox_request_sender
                .lock()
                .unwrap_or_else(|slot| slot.into_inner()) = PeerInboxRequestRoute::Sync;
            PeerInboxAckSender::Sync(ctx)
        }
    }
}

fn request_codex_renudge_after_ack(
    ctx: &PeerCtx,
    remaining: usize,
    next: &QueuedPeerMessage,
) -> &'static str {
    if remaining == 0 || ctx.client_kind != PeerClientKind::Codex {
        return if remaining == 0 {
            "skipped_none_pending"
        } else {
            "skipped_not_codex"
        };
    }
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return "skipped_detached";
    };
    let Ok(next_from_pane) = next.from_id.parse::<usize>() else {
        log_stderr(&format!(
            "cannot request Codex re-nudge for non-numeric sender id {:?}",
            next.from_id
        ));
        return "rejected";
    };
    let request = Request::PeerInboxHeadAcknowledged {
        pane_id: *pane_id,
        remaining,
        next_from_pane,
        next_from_name: next.from_name.clone(),
        next_from_kind: next.from_kind,
    };

    #[cfg(test)]
    if let Some(sink) = &ctx.request_sink {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(request);
        return match ctx
            .request_sink_response
            .clone()
            .unwrap_or_else(Response::ok_unit)
        {
            Response::Ok { .. } => "sent",
            other => {
                log_stderr(&format!("Codex re-nudge request returned: {other:?}"));
                "rejected"
            }
        };
    }

    let result = client::send_request(endpoint, &request);
    match result {
        Ok(Response::Ok { .. }) => "sent",
        Ok(other) => {
            log_stderr(&format!("Codex re-nudge request returned: {other:?}"));
            "rejected"
        }
        Err(error) => {
            log_stderr(&format!("Codex re-nudge request failed: {error}"));
            "rejected"
        }
    }
}

fn notify_peer_inbox_consumed(ctx: &PeerCtx, delivery_id: u64) -> bool {
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return false;
    };
    let request = Request::PeerInboxConsumed {
        pane_id: *pane_id,
        delivery_id,
    };
    #[cfg(test)]
    let result = if let Some(sink) = &ctx.request_sink {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(request);
        Ok(ctx
            .request_sink_response
            .clone()
            .unwrap_or_else(Response::ok_unit))
    } else {
        client::send_request(endpoint, &request)
    };
    #[cfg(not(test))]
    let result = client::send_request(endpoint, &request);
    match &result {
        Ok(Response::Ok { .. }) => {}
        Ok(other) => log_stderr(&format!("peer inbox consumed request returned: {other:?}")),
        Err(error) => log_stderr(&format!("peer inbox consumed request failed: {error}")),
    }
    let request_succeeded = matches!(result, Ok(Response::Ok { .. }));
    if let Some(path) = ctx.debug_log_path.as_deref() {
        let (ok, error) = match &result {
            Ok(Response::Ok { .. }) => (true, None),
            Ok(other) => (false, Some(format!("unexpected response: {other:?}"))),
            Err(error) => (false, Some(error.to_string())),
        };
        append_peer_debug_record(
            path,
            Some(*pane_id),
            json!({
                "action": "peer_inbox_consumed_sent",
                "delivery_id": delivery_id,
                "ok": ok,
                "error": error,
            }),
        );
    }
    request_succeeded
}

const UNREPORTED_CONSUMED_CAP: usize = 256;

fn retain_unreported_consumed(ctx: &PeerCtx, delivery_id: u64) {
    let mut ids = ctx
        .unreported_consumed
        .lock()
        .unwrap_or_else(|items| items.into_inner());
    if !ids.contains(&delivery_id) {
        ids.push_back(delivery_id);
        if ids.len() > UNREPORTED_CONSUMED_CAP {
            ids.pop_front();
            ctx.unreported_consumed_overflow
                .fetch_add(1, Ordering::AcqRel);
        }
    }
    let count = ids.len();
    if let Some(path) = ctx.debug_log_path.as_deref() {
        append_peer_debug_record(
            path,
            peer_ctx_pane_id(ctx),
            json!({
                "action": "peer_inbox_consumed_unreported",
                "delivery_id": delivery_id,
                "count": count,
            }),
        );
    }
}

fn mark_consumed_reported(ctx: &PeerCtx, delivery_id: u64) {
    let mut ids = ctx
        .unreported_consumed
        .lock()
        .unwrap_or_else(|items| items.into_inner());
    if let Some(position) = ids.iter().position(|id| *id == delivery_id) {
        ids.remove(position);
    }
    let count = ids.len();
    if let Some(path) = ctx.debug_log_path.as_deref() {
        append_peer_debug_record(
            path,
            peer_ctx_pane_id(ctx),
            json!({
                "action": "peer_inbox_consumed_reported",
                "delivery_id": delivery_id,
                "count": count,
            }),
        );
    }
}

fn request_peer_inbox_consumed(ctx: &PeerCtx, delivery_id: Option<u64>) -> &'static str {
    let Some(delivery_id) = delivery_id else {
        return "skipped_no_delivery_id";
    };
    let route = ctx
        .peer_inbox_request_sender
        .lock()
        .unwrap_or_else(|slot| slot.into_inner())
        .clone();
    match route {
        PeerInboxRequestRoute::Unavailable => "rejected_sender_unavailable",
        PeerInboxRequestRoute::Async { sender, depth } => {
            let queue_depth = depth.fetch_add(1, Ordering::AcqRel) + 1;
            if let Some(path) = ctx.debug_log_path.as_deref() {
                append_peer_debug_record(
                    path,
                    peer_ctx_pane_id(ctx),
                    json!({
                        "action": "peer_inbox_consumed_queued",
                        "delivery_id": delivery_id,
                        "depth": queue_depth,
                    }),
                );
            }
            match sender.send(PeerInboxQueuedRequest::Consumed(delivery_id)) {
                Ok(()) => "sent",
                Err(_) => {
                    depth.fetch_sub(1, Ordering::AcqRel);
                    "rejected_queue_closed"
                }
            }
        }
        PeerInboxRequestRoute::Sync => {
            if notify_peer_inbox_consumed(ctx, delivery_id) {
                mark_consumed_reported(ctx, delivery_id);
                "sent_sync_fallback"
            } else {
                "rejected_sync_fallback"
            }
        }
    }
}

fn reconcile_peer_inbox(ctx: &PeerCtx) -> bool {
    if ctx.client_kind != PeerClientKind::Codex {
        return true;
    }
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return false;
    };
    let (held, held_overflow) = {
        let inbox = ctx.inbox.lock().unwrap_or_else(|state| state.into_inner());
        let ids: Vec<u64> = inbox
            .messages
            .iter()
            .filter_map(|entry| entry.message.delivery_id)
            .take(UNREPORTED_CONSUMED_CAP)
            .collect();
        let total = inbox
            .messages
            .iter()
            .filter(|entry| entry.message.delivery_id.is_some())
            .count();
        (ids, total.saturating_sub(UNREPORTED_CONSUMED_CAP))
    };
    let consumed = ctx
        .unreported_consumed
        .lock()
        .unwrap_or_else(|items| items.into_inner())
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let consumed_overflow = ctx.unreported_consumed_overflow.load(Ordering::Acquire);
    #[cfg(test)]
    if let Some(barrier) = &ctx.reconcile_snapshot_barrier {
        barrier.wait();
        barrier.wait();
    }
    let request = Request::PeerInboxReconcile {
        pane_id: *pane_id,
        held: held.clone(),
        consumed: consumed.clone(),
        held_overflow,
        consumed_overflow,
    };
    #[cfg(test)]
    let result = if let Some(sink) = &ctx.request_sink {
        sink.lock()
            .unwrap_or_else(|items| items.into_inner())
            .push(request);
        Ok(ctx
            .request_sink_response
            .clone()
            .unwrap_or_else(Response::ok_unit))
    } else {
        client::send_request(endpoint, &request)
    };
    #[cfg(not(test))]
    let result = client::send_request(endpoint, &request);
    let ok = matches!(result, Ok(Response::Ok { .. }));
    if ok {
        let mut unreported = ctx
            .unreported_consumed
            .lock()
            .unwrap_or_else(|items| items.into_inner());
        unreported.retain(|id| !consumed.contains(id));
        ctx.unreported_consumed_overflow
            .fetch_sub(consumed_overflow, Ordering::AcqRel);
    }
    if let Some(path) = ctx.debug_log_path.as_deref() {
        append_peer_debug_record(
            path,
            Some(*pane_id),
            json!({
                "action": "peer_inbox_reconciled",
                "ok": ok,
                "held_count": held.len(),
                "consumed_count": consumed.len(),
                "held_overflow": held_overflow,
                "consumed_overflow": consumed_overflow,
            }),
        );
    }
    ok
}

fn log_peer_inbox_ack_sent(
    path: &Path,
    pane_id: usize,
    delivery_id: u64,
    ok: bool,
    error: Option<&str>,
) {
    append_peer_debug_record(
        path,
        Some(pane_id),
        json!({
            "action": "peer_inbox_ack_sent",
            "delivery_id": delivery_id,
            "ok": ok,
            "error": error,
        }),
    );
}

const PEER_RECEIPT_CACHE_CAP: usize = 4096;

#[derive(Default)]
struct PeerReceiptCache {
    ids: VecDeque<u64>,
    set: std::collections::HashSet<u64>,
}

impl PeerReceiptCache {
    fn contains(&self, delivery_id: u64) -> bool {
        self.set.contains(&delivery_id)
    }

    fn insert(&mut self, delivery_id: u64) {
        if !self.set.insert(delivery_id) {
            return;
        }
        self.ids.push_back(delivery_id);
        while self.ids.len() > PEER_RECEIPT_CACHE_CAP {
            if let Some(expired) = self.ids.pop_front() {
                self.set.remove(&expired);
            }
        }
    }
}

fn retain_peer_delivery_once(
    cache: &mut PeerReceiptCache,
    delivery_id: Option<u64>,
    retain: impl FnOnce() -> bool,
) -> bool {
    if delivery_id.is_some_and(|id| cache.contains(id)) {
        return true;
    }
    if !retain() {
        return false;
    }
    if let Some(delivery_id) = delivery_id {
        cache.insert(delivery_id);
    }
    true
}

#[derive(Default)]
struct PushState {
    initialized: bool,
    initialized_at: Option<Instant>,
    subscribed: bool,
    ready_generation: u64,
    ready_scheduled: bool,
    ready_announced: bool,
    pending: VecDeque<PendingPushFrame>,
}

#[derive(Debug)]
struct PendingPushFrame {
    value: Value,
    delivery_id: Option<u64>,
    frame_kind: &'static str,
}

const PUSH_PENDING_CAP: usize = 256;

type PushSink = Arc<Mutex<PushState>>;

/// Runtime context shared between the main stdio loop and the inbox
/// subscriber thread. Cloneable because both halves read the same
/// `(pane_id, endpoint)` pair to contact the renga server and the
/// same [`EventSink`] for `poll_events` buffering.
#[derive(Clone)]
struct PeerCtx {
    mode: Mode,
    client_kind: PeerClientKind,
    events: EventSink,
    inbox: InboxSink,
    push: PushSink,
    ready_publish_lock: Arc<Mutex<()>>,
    peer_inbox_request_sender: Arc<Mutex<PeerInboxRequestRoute>>,
    unreported_consumed: Arc<Mutex<VecDeque<u64>>>,
    unreported_consumed_overflow: Arc<AtomicUsize>,
    debug_log_path: Option<PathBuf>,
    #[cfg(test)]
    request_sink: Option<Arc<Mutex<Vec<Request>>>>,
    #[cfg(test)]
    request_sink_response: Option<Response>,
    #[cfg(test)]
    reconcile_snapshot_barrier: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    push_ready_delay: Duration,
}

/// Soft cap on the per-process lifecycle event buffer used by
/// `poll_events`. Older entries are evicted on overflow; a caller that
/// falls behind by more than this many events will miss the oldest
/// ones. The upstream `EventsDropped` meta-event (emitted when the
/// subscribe channel itself drops) still flows through as a regular
/// buffered event so the caller can notice.
const EVENT_BUFFER_CAP: usize = 4096;

/// Default `timeout_ms` for `poll_events` when the caller doesn't
/// specify one. Long enough to absorb a quiet period without spinning,
/// short enough to keep the stdio dispatcher responsive if Claude Code
/// wants to interleave tool calls.
const POLL_DEFAULT_TIMEOUT_MS: u64 = 2000;

/// Hard cap on `timeout_ms` regardless of what the caller requests.
/// A single `poll_events` call blocks the mcp-peer stdio dispatcher
/// for its duration — this bound keeps an unresponsive client from
/// wedging the whole MCP.
const POLL_MAX_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug)]
struct SeqEvent {
    seq: u64,
    value: Value,
}

/// Ring buffer of lifecycle events assigned monotonic 1-based
/// sequence numbers. `seq = 0` is the "nothing yet" sentinel returned
/// as `next_since` when the caller polls an empty stream.
#[derive(Default)]
struct EventBuffer {
    events: VecDeque<SeqEvent>,
    /// Seq of the most recently pushed event. `0` before any event.
    last_seq: u64,
}

impl EventBuffer {
    fn push(&mut self, value: Value) -> u64 {
        self.last_seq = self.last_seq.saturating_add(1);
        let seq = self.last_seq;
        self.events.push_back(SeqEvent { seq, value });
        while self.events.len() > EVENT_BUFFER_CAP {
            self.events.pop_front();
        }
        seq
    }
}

type EventSink = Arc<(Mutex<EventBuffer>, Condvar)>;

fn new_event_sink() -> EventSink {
    Arc::new((Mutex::new(EventBuffer::default()), Condvar::new()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedPeerMessage {
    delivery_id: Option<u64>,
    from_id: String,
    from_name: Option<String>,
    from_kind: Option<PeerClientKind>,
    body: String,
    sent_at: String,
}

#[derive(Debug)]
struct InboxEntry {
    message_id: String,
    ack_token: String,
    message: QueuedPeerMessage,
}

#[derive(Default, Debug)]
struct InboxState {
    messages: VecDeque<InboxEntry>,
    next_message_id: u64,
}

type InboxSink = Arc<Mutex<InboxState>>;

const CHECK_MESSAGES_DEFAULT_RESPONSE_BYTES: usize = 4096;
const CHECK_MESSAGES_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

fn new_inbox_sink() -> InboxSink {
    Arc::new(Mutex::new(InboxState::default()))
}

#[derive(Clone)]
enum Mode {
    /// Running inside a renga pane with a reachable IPC endpoint.
    Connected {
        pane_id: usize,
        endpoint: EndpointName,
    },
    /// Missing `RENGA_PANE_ID` or `RENGA_SOCKET`. Tools still respond
    /// but with empty/no-op payloads so `claude` launched outside
    /// renga doesn't log MCP errors on startup.
    Detached { reason: String },
}

impl PeerCtx {
    fn load() -> Self {
        let events = new_event_sink();
        let inbox = new_inbox_sink();
        let push = Arc::new(Mutex::new(PushState::default()));
        let ready_publish_lock = Arc::new(Mutex::new(()));
        let debug_log_path = std::env::var_os(ENV_DEBUG_CODEX_PEER_LOG).map(PathBuf::from);
        let client_kind_raw = std::env::var(ENV_CLIENT_KIND);
        let client_kind = client_kind_raw
            .as_ref()
            .ok()
            .and_then(|s| parse_client_kind(s))
            .unwrap_or(PeerClientKind::Claude);
        log_client_kind_resolution(debug_log_path.as_deref(), &client_kind_raw, client_kind);
        let pane_id = match std::env::var(ENV_PANE_ID) {
            Ok(s) => match s.parse::<usize>() {
                Ok(v) => v,
                Err(_) => {
                    return PeerCtx {
                        mode: Mode::Detached {
                            reason: format!("{ENV_PANE_ID} is set but not a valid usize: {s:?}"),
                        },
                        events,
                        inbox,
                        push,
                        ready_publish_lock,
                        peer_inbox_request_sender: Arc::new(Mutex::new(
                            PeerInboxRequestRoute::Unavailable,
                        )),
                        unreported_consumed_overflow: Arc::new(AtomicUsize::new(0)),
                        unreported_consumed: Arc::new(Mutex::new(VecDeque::new())),
                        client_kind,
                        debug_log_path,
                        #[cfg(test)]
                        request_sink: None,
                        #[cfg(test)]
                        request_sink_response: None,
                        #[cfg(test)]
                        reconcile_snapshot_barrier: None,
                        #[cfg(test)]
                        push_ready_delay: PUSH_READY_DELAY,
                    };
                }
            },
            Err(_) => {
                return PeerCtx {
                    mode: Mode::Detached {
                        reason: format!(
                            "{ENV_PANE_ID} not set — Claude Code was not launched by renga"
                        ),
                    },
                    events,
                    inbox,
                    push,
                    ready_publish_lock,
                    peer_inbox_request_sender: Arc::new(Mutex::new(
                        PeerInboxRequestRoute::Unavailable,
                    )),
                    unreported_consumed_overflow: Arc::new(AtomicUsize::new(0)),
                    unreported_consumed: Arc::new(Mutex::new(VecDeque::new())),
                    client_kind,
                    debug_log_path,
                    #[cfg(test)]
                    request_sink: None,
                    #[cfg(test)]
                    request_sink_response: None,
                    #[cfg(test)]
                    reconcile_snapshot_barrier: None,
                    #[cfg(test)]
                    push_ready_delay: PUSH_READY_DELAY,
                };
            }
        };
        match endpoint_from_env() {
            Ok(endpoint) => PeerCtx {
                mode: Mode::Connected { pane_id, endpoint },
                events,
                inbox,
                push,
                ready_publish_lock,
                peer_inbox_request_sender: Arc::new(Mutex::new(PeerInboxRequestRoute::Unavailable)),
                unreported_consumed_overflow: Arc::new(AtomicUsize::new(0)),
                unreported_consumed: Arc::new(Mutex::new(VecDeque::new())),
                client_kind,
                debug_log_path,
                #[cfg(test)]
                request_sink: None,
                #[cfg(test)]
                request_sink_response: None,
                #[cfg(test)]
                reconcile_snapshot_barrier: None,
                #[cfg(test)]
                push_ready_delay: PUSH_READY_DELAY,
            },
            Err(e) => PeerCtx {
                mode: Mode::Detached {
                    reason: format!("{ENV_SOCKET} missing or invalid: {e}"),
                },
                events,
                inbox,
                push,
                ready_publish_lock,
                peer_inbox_request_sender: Arc::new(Mutex::new(PeerInboxRequestRoute::Unavailable)),
                unreported_consumed_overflow: Arc::new(AtomicUsize::new(0)),
                unreported_consumed: Arc::new(Mutex::new(VecDeque::new())),
                client_kind,
                debug_log_path,
                #[cfg(test)]
                request_sink: None,
                #[cfg(test)]
                request_sink_response: None,
                #[cfg(test)]
                reconcile_snapshot_barrier: None,
                #[cfg(test)]
                push_ready_delay: PUSH_READY_DELAY,
            },
        }
    }
}

fn parse_client_kind(raw: &str) -> Option<PeerClientKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(PeerClientKind::Claude),
        "codex" => Some(PeerClientKind::Codex),
        _ => None,
    }
}

fn log_client_kind_resolution(
    debug_log_path: Option<&Path>,
    client_kind_raw: &std::result::Result<String, std::env::VarError>,
    client_kind: PeerClientKind,
) {
    let Some(path) = debug_log_path else {
        return;
    };

    let (client_kind_env_state, client_kind_raw_value, client_kind_raw_is_unicode) =
        match client_kind_raw {
            Ok(raw) if parse_client_kind(raw).is_some() => {
                ("parsed", Some(raw.clone()), Some(true))
            }
            Ok(raw) => ("present-but-unparseable", Some(raw.clone()), Some(true)),
            Err(std::env::VarError::NotPresent) => ("absent", None, None),
            Err(std::env::VarError::NotUnicode(raw)) => (
                "present-but-unparseable",
                Some(format!("{raw:?}")),
                Some(false),
            ),
        };
    let pane_id_raw = std::env::var(ENV_PANE_ID).ok();
    let pane_id = pane_id_raw
        .as_deref()
        .and_then(|raw| raw.parse::<usize>().ok());
    let exe_path = std::env::current_exe().ok();
    let exe_modified_unix_ms = exe_path
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis());
    let args: Vec<String> = std::env::args().collect();
    let record = json!({
        "action": "client_kind_resolved",
        "version": env!("CARGO_PKG_VERSION"),
        "executable_path": exe_path,
        "executable_modified_unix_ms": exe_modified_unix_ms,
        "args_summary": {
            "count": args.len(),
            "subcommand": args.get(1),
        },
        "pane_id": pane_id,
        "renga_peer_client_kind_state": client_kind_env_state,
        "renga_peer_client_kind_raw": client_kind_raw_value,
        "renga_peer_client_kind_raw_is_unicode": client_kind_raw_is_unicode,
        "resolved_client_kind": kind_label(client_kind),
        "receive_mode": receive_mode_label(client_kind.receive_mode()),
        "renga_pane_id_present": std::env::var_os(ENV_PANE_ID).is_some(),
        "renga_socket_present": std::env::var_os(ENV_SOCKET).is_some(),
        "renga_token_present": std::env::var_os(ENV_TOKEN).is_some(),
    });
    append_peer_debug_record(path, pane_id, record);
}

fn append_peer_debug_record(path: &Path, pane_id: Option<usize>, mut record: Value) {
    let Some(record) = record.as_object_mut() else {
        return;
    };
    let timestamp_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let record_sequence =
        PEER_DEBUG_RECORD_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    record.insert("timestamp_unix_ms".to_string(), json!(timestamp_unix_ms));
    record.insert("component".to_string(), json!("mcp_peer"));
    record.insert("process_id".to_string(), json!(std::process::id()));
    record.insert("record_sequence".to_string(), json!(record_sequence));
    record.insert("pane_id".to_string(), json!(pane_id));
    let failures = take_peer_debug_write_failures();
    record.insert(
        "trace_write_failures_since_last".to_string(),
        json!(failures),
    );
    // serde_json::Value has no fallible serialization cases. Keep the
    // defensive branch non-panicking and return the reserved count.
    let mut line = match serde_json::to_vec(record) {
        Ok(line) => line,
        Err(_) => {
            PEER_DEBUG_WRITE_FAILURES.fetch_add(failures, std::sync::atomic::Ordering::Relaxed);
            return;
        }
    };
    line.push(b'\n');
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        PEER_DEBUG_WRITE_FAILURES.fetch_add(
            failures.saturating_add(1),
            std::sync::atomic::Ordering::Relaxed,
        );
        return;
    };
    record_peer_write_result(failures, file.write_all(&line));
}

fn take_peer_debug_write_failures() -> u64 {
    PEER_DEBUG_WRITE_FAILURES.swap(0, std::sync::atomic::Ordering::AcqRel)
}

fn record_peer_write_result(reported_failures: u64, result: std::io::Result<()>) {
    if result.is_err() {
        PEER_DEBUG_WRITE_FAILURES.fetch_add(
            reported_failures.saturating_add(1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

// ── stdio JSON-RPC frame plumbing ─────────────────────────────

fn write_frame(value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value).context("serialize frame")?;
    line.push('\n');
    let out = io::stdout();
    let mut guard = out.lock();
    guard
        .write_all(line.as_bytes())
        .context("write frame to stdout")?;
    guard.flush().context("flush stdout")?;
    Ok(())
}

fn deliver_push_frame(
    ctx: &PeerCtx,
    value: Value,
    delivery_id: Option<u64>,
    frame_kind: &'static str,
) -> bool {
    deliver_push_frame_with(ctx, value, delivery_id, frame_kind, write_frame)
}

fn deliver_push_frame_with<F>(
    ctx: &PeerCtx,
    value: Value,
    delivery_id: Option<u64>,
    frame_kind: &'static str,
    mut emit: F,
) -> bool
where
    F: FnMut(&Value) -> Result<()>,
{
    let mut state = ctx.push.lock().unwrap_or_else(|p| p.into_inner());
    if !state.initialized {
        if state.pending.len() >= PUSH_PENDING_CAP {
            // PeerInbox delivery stays queued in the App until push
            // initialization, so this cap normally only affects repeated
            // diagnostic notices such as EventsDropped before initialization.
            log_stderr("push notification buffer full before initialized; dropping newest notice");
            log_push_frame(
                ctx,
                "push_frame_dropped_cap",
                delivery_id,
                frame_kind,
                || json!({ "pending_len": state.pending.len() }),
            );
            return false;
        }
        state.pending.push_back(PendingPushFrame {
            value,
            delivery_id,
            frame_kind,
        });
        log_push_frame(
            ctx,
            "push_frame_buffered",
            delivery_id,
            frame_kind,
            || json!({ "pending_len_after": state.pending.len() }),
        );
        return true;
    }
    // Keep the push lock through the write so a concurrent initialized
    // flush cannot be overtaken by a newly arrived notification.
    if let Err(e) = emit(&value) {
        let initialized_age_ms = state
            .initialized_at
            .map_or(0, |initialized_at| initialized_at.elapsed().as_millis());
        let error = e.to_string();
        log_stderr(&format!("failed to push channel notification: {error}"));
        log_push_frame(
            ctx,
            "push_frame_emit_failed",
            delivery_id,
            frame_kind,
            || json!({ "error": error, "via": "direct", "initialized_age_ms": initialized_age_ms }),
        );
        return false;
    }
    let initialized_age_ms = state
        .initialized_at
        .map_or(0, |initialized_at| initialized_at.elapsed().as_millis());
    log_push_frame(ctx, "push_frame_emitted", delivery_id, frame_kind, || {
        json!({
            "initialized_age_ms": initialized_age_ms,
            "via": "direct",
        })
    });
    true
}

fn log_push_frame<F>(
    ctx: &PeerCtx,
    action: &'static str,
    delivery_id: Option<u64>,
    frame_kind: &'static str,
    build_fields: F,
) where
    F: FnOnce() -> Value,
{
    let Some(path) = ctx.debug_log_path.as_deref() else {
        return;
    };
    let mut record = build_fields();
    if let Some(object) = record.as_object_mut() {
        object.insert("action".into(), json!(action));
        object.insert("delivery_id".into(), json!(delivery_id));
        object.insert("frame_kind".into(), json!(frame_kind));
    }
    let pane_id = match &ctx.mode {
        Mode::Connected { pane_id, .. } => Some(*pane_id),
        Mode::Detached { .. } => None,
    };
    append_peer_debug_record(path, pane_id, record);
}

fn log_push_lifecycle<F>(ctx: &PeerCtx, action: &'static str, build_fields: F)
where
    F: FnOnce() -> Value,
{
    let Some(path) = ctx.debug_log_path.as_deref() else {
        return;
    };
    let mut record = build_fields();
    if let Some(object) = record.as_object_mut() {
        object.insert("action".into(), json!(action));
    }
    let pane_id = match &ctx.mode {
        Mode::Connected { pane_id, .. } => Some(*pane_id),
        Mode::Detached { .. } => None,
    };
    append_peer_debug_record(path, pane_id, record);
}

fn mark_push_initialized(ctx: &PeerCtx) -> bool {
    mark_push_initialized_with(ctx, write_frame)
}

fn mark_push_initialized_with<F>(ctx: &PeerCtx, mut emit: F) -> bool
where
    F: FnMut(&Value) -> Result<()>,
{
    let mut state = ctx.push.lock().unwrap_or_else(|p| p.into_inner());
    if !state.initialized {
        state.initialized = true;
        state.initialized_at = Some(Instant::now());
    }
    let subscribed_at_that_time = state.subscribed;
    let mut outcomes = ctx
        .debug_log_path
        .is_some()
        .then(|| Vec::with_capacity(state.pending.len()));
    let mut flushed_count = 0usize;
    let mut failed_count = 0usize;
    while let Some(frame) = state.pending.pop_front() {
        let result = emit(&frame.value).map_err(|error| error.to_string());
        let initialized_age_ms = state
            .initialized_at
            .map_or(0, |initialized_at| initialized_at.elapsed().as_millis());
        match &result {
            Ok(()) => flushed_count += 1,
            Err(error) => {
                failed_count += 1;
                log_stderr(&format!("failed to flush channel notification: {error}"));
            }
        }
        if let Some(outcomes) = outcomes.as_mut() {
            outcomes.push((frame, result, initialized_age_ms));
        }
    }
    log_push_lifecycle(ctx, "push_initialized", || {
        json!({
            "flushed_count": flushed_count,
            "failed_count": failed_count,
            "subscribed_at_that_time": subscribed_at_that_time,
        })
    });
    for (frame, result, initialized_age_ms) in outcomes.into_iter().flatten() {
        match result {
            Ok(()) => log_push_frame(
                ctx,
                "push_frame_emitted",
                frame.delivery_id,
                frame.frame_kind,
                || {
                    json!({
                        "initialized_age_ms": initialized_age_ms,
                        "via": "initialized_flush",
                    })
                },
            ),
            Err(error) => log_push_frame(
                ctx,
                "push_frame_emit_failed",
                frame.delivery_id,
                frame.frame_kind,
                || {
                    json!({
                        "error": error,
                        "initialized_age_ms": initialized_age_ms,
                        "via": "initialized_flush",
                    })
                },
            ),
        }
    }
    state.subscribed
}

fn mark_push_subscribed(ctx: &PeerCtx, subscribed: bool) -> bool {
    let mut state = ctx.push.lock().unwrap_or_else(|p| p.into_inner());
    let changed = state.subscribed != subscribed;
    state.subscribed = subscribed;
    if !subscribed && changed {
        state.ready_generation = state.ready_generation.saturating_add(1);
        state.ready_scheduled = false;
        state.ready_announced = false;
    }
    if changed {
        log_push_lifecycle(ctx, "push_subscribed", || {
            json!({
                "subscribed": subscribed,
                "initialized_at_that_time": state.initialized,
            })
        });
    }
    state.initialized && state.subscribed
}

#[derive(Clone, Copy, Debug)]
struct DeferredPushReady {
    generation: u64,
    delay: Duration,
}

fn configured_push_ready_delay(ctx: &PeerCtx) -> Duration {
    #[cfg(test)]
    {
        ctx.push_ready_delay
    }
    #[cfg(not(test))]
    {
        let _ = ctx;
        PUSH_READY_DELAY
    }
}

fn prepare_deferred_push_ready_at(ctx: &PeerCtx, now: Instant) -> Option<DeferredPushReady> {
    if ctx.client_kind.receive_mode() != ipc::PeerReceiveMode::Push {
        return None;
    }
    let mut state = ctx.push.lock().unwrap_or_else(|p| p.into_inner());
    if !state.initialized || !state.subscribed || state.ready_scheduled || state.ready_announced {
        return None;
    }
    let initialized_at = state.initialized_at?;
    let target_delay = configured_push_ready_delay(ctx);
    let elapsed = now.saturating_duration_since(initialized_at);
    let delay = target_delay.saturating_sub(elapsed);
    state.ready_scheduled = true;
    let generation = state.ready_generation;
    drop(state);
    if !delay.is_zero() {
        log_push_lifecycle(ctx, "peer_set_ready_deferred", || {
            json!({
                "client_kind": kind_label(ctx.client_kind),
                "delay_ms": delay.as_millis(),
            })
        });
    }
    Some(DeferredPushReady { generation, delay })
}

fn finish_deferred_push_ready(ctx: &PeerCtx, deferred: DeferredPushReady) -> bool {
    let mut state = ctx.push.lock().unwrap_or_else(|p| p.into_inner());
    if state.ready_generation != deferred.generation || !state.initialized || !state.subscribed {
        return false;
    }
    state.ready_scheduled = false;
    if state.ready_announced {
        return false;
    }
    state.ready_announced = true;
    true
}

fn schedule_deferred_push_ready(ctx: &PeerCtx) {
    let Some(deferred) = prepare_deferred_push_ready_at(ctx, Instant::now()) else {
        return;
    };
    if deferred.delay.is_zero() {
        let publish_lock = ctx.ready_publish_lock.clone();
        // Keep readiness publications ordered across the timer and subscriber
        // threads so a stale true cannot follow a disconnect's false.
        let _publish_guard = publish_lock.lock().unwrap_or_else(|lock| lock.into_inner());
        if finish_deferred_push_ready(ctx, deferred) {
            set_client_ready(ctx, true);
        }
        return;
    }
    let deferred_ctx = ctx.clone();
    if let Err(error) = thread::Builder::new()
        .name("renga-mcp-peer-ready-delay".into())
        .spawn(move || {
            thread::sleep(deferred.delay);
            let publish_lock = deferred_ctx.ready_publish_lock.clone();
            // The IPC runs under this lock so a disconnect cannot publish
            // false and then be overwritten by this older timer's true.
            let _publish_guard = publish_lock.lock().unwrap_or_else(|lock| lock.into_inner());
            if finish_deferred_push_ready(&deferred_ctx, deferred) {
                set_client_ready(&deferred_ctx, true);
            }
        })
    {
        let mut state = ctx.push.lock().unwrap_or_else(|p| p.into_inner());
        if state.ready_generation == deferred.generation {
            state.ready_scheduled = false;
        }
        log_stderr(&format!("failed to spawn peer readiness delay: {error}"));
    }
}

fn revoke_push_ready(ctx: &PeerCtx) {
    let publish_lock = ctx.ready_publish_lock.clone();
    // A disconnect may wait for an in-flight readiness IPC, but no live
    // subscriber can lose work during this teardown-only wait.
    let _publish_guard = publish_lock.lock().unwrap_or_else(|lock| lock.into_inner());
    mark_push_subscribed(ctx, false);
    set_client_ready(ctx, false);
}

fn publish_ready_after_subscribe(ctx: &PeerCtx) {
    if ctx.client_kind.receive_mode() == ipc::PeerReceiveMode::Pull {
        set_client_ready(ctx, true);
    } else if mark_push_subscribed(ctx, true) {
        schedule_deferred_push_ready(ctx);
    }
}

fn ok_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err_response(id: &Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn tool_text_result(text: &str) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": false })
}

fn queue_pull_message(inbox: &InboxSink, message: QueuedPeerMessage) {
    let mut state = inbox.lock().unwrap_or_else(|p| p.into_inner());
    state.next_message_id = state.next_message_id.saturating_add(1);
    let message_id = format!("m{}", state.next_message_id);
    let ack_token = new_ack_token();
    state.messages.push_back(InboxEntry {
        message_id,
        ack_token,
        message,
    });
}

fn new_ack_token() -> String {
    let mut bytes = [0u8; 16];
    if let Err(error) = getrandom::getrandom(&mut bytes) {
        use std::hash::{Hash, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static FALLBACK_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = FALLBACK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut first = std::collections::hash_map::DefaultHasher::new();
        timestamp.hash(&mut first);
        std::process::id().hash(&mut first);
        sequence.hash(&mut first);
        (&bytes as *const [u8; 16] as usize).hash(&mut first);
        let first = first.finish();
        let mut second = std::collections::hash_map::DefaultHasher::new();
        first.hash(&mut second);
        timestamp.rotate_left(47).hash(&mut second);
        let second = second.finish();
        bytes[..8].copy_from_slice(&first.to_le_bytes());
        bytes[8..].copy_from_slice(&second.to_le_bytes());
        log_stderr(&format!(
            "OS randomness unavailable for peer receipt token; using process-local fallback: {error}"
        ));
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ── channel notification (the whole point of #97) ─────────────

/// Build the `notifications/claude/channel` push that makes a peer
/// message show up as `<channel source="renga-peers">...</channel>`
/// in the receiver's context. The `source=` attribute is derived by
/// Claude Code from our `serverInfo.name`, not from this payload, so
/// `params.meta` here only carries sender metadata.
///
/// Claude Code currently injects channel notifications into a
/// user-slot turn, which the transcript renders with a `Human:`
/// prefix even though the content is from a peer. To keep operators
/// from mistaking peer chatter for things the human typed, the body
/// is wrapped with a loud banner that's obviously machine-generated
/// (uppercase, emoji, explicit "not from user"). See renga#221.
fn channel_notification(body: &str, from_id: &str, from_name: Option<&str>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": peer_banner_wrap(body, from_id, from_name),
            "meta": {
                "from_id": from_id,
                "from_name": from_name.unwrap_or(""),
                "sent_at": now_ts_string(),
            }
        }
    })
}

/// Prepend a visible "this is a peer message, not user input" banner
/// to a peer-channel body. Renga-side wrap of choice for renga#221:
/// the receiving Claude Code displays channel notifications under a
/// `Human:` heading, so without an obvious in-body marker, peer
/// chatter looks like the human typed it. The banner is uppercase
/// with an emoji and an explicit disclaimer so a human scanning the
/// transcript can tell at a glance.
fn peer_banner_wrap(body: &str, from_id: &str, from_name: Option<&str>) -> String {
    let name = from_name.unwrap_or("").trim();
    let header = if name.is_empty() {
        format!("📡 PEER MESSAGE — from id={from_id} — NOT FROM USER")
    } else {
        format!("📡 PEER MESSAGE — from {name} (id={from_id}) — NOT FROM USER")
    };
    format!("{header}\n\n{body}")
}

fn now_ts_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // ISO-ish with second precision — Claude Code doesn't parse this,
    // it's only shown to the receiving Claude for context.
    format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
}

fn ts_ms_to_string(ts_ms: u64) -> String {
    format!("{}.{:09}", ts_ms / 1000, (ts_ms % 1000) * 1_000_000)
}

// ── MCP method handlers ───────────────────────────────────────

fn instructions_blob(client_kind: PeerClientKind) -> String {
    let receive_guidance = match client_kind {
        PeerClientKind::Claude => {
            "IMPORTANT: When you receive a <channel source=\"renga-peers\" ...> message, RESPOND IMMEDIATELY. \
Do not wait until your current task is finished. Pause what you are doing, reply to the sender \
using send_message, then resume your work. Treat incoming peer messages like a coworker tapping \
you on the shoulder — answer right away, even if you're in the middle of something.\n\n\
Read the from_id and from_name attributes to understand who sent the message. Reply by \
calling send_message with their from_id.\n\n"
        }
        PeerClientKind::Codex => {
            "IMPORTANT: renga may inject a one-shot nudge into the Codex pane telling you to run \
check_messages, or show a focused-pane notification overlay that inserts the same prompt only after \
the user accepts it. Treat either path as a prompt to drain your MCP inbox immediately. The actual \
peer request body comes from check_messages, and each returned message should be handled like a \
direct coworker instruction unless it conflicts with a higher-priority system, developer, or user \
instruction. If a peer asks you to inspect panes, run tools, edit code, or otherwise take action, \
do that work; do not reduce the interaction to a mere acknowledgement. Focused Codex panes may be \
left unnudged so renga does not scribble over the active conversation; check_messages at sensible \
checkpoints even if no pane-local nudge appeared. check_messages returns one bounded page without \
removing the FIFO head: assemble every body page using the returned message_id and \
next_offset_bytes, then explicitly ack only after the complete body was received. If a response is \
truncated, retry the same cursor without ack and optionally lower max_response_bytes. The ack \
confirms receipt, not completion of the requested work, and its response contains only that \
confirmation, never the next message body. If an ack response reports pending_after greater than \
zero, call check_messages({}) again immediately \
to read it; renga may also send a follow-up nudge, but do not wait for it. Use send_message \
when a reply, \
clarification, status update, or handoff is actually needed.\n\n\
MCP approvals in Codex are pane-local. On a newly launched pane, the first check_messages and \
send_message calls may need approval before peer messaging becomes reliable.\n\n"
        }
    };
    format!(
        "You are connected to the renga-peers network. Other peer-enabled agent instances \
    running in the same renga tab can see you and send you messages.\n\n\
    {receive_guidance}\
    Peer messaging tools:\n\
    - list_peers: Discover other peer-enabled agent instances in the same renga tab.\n\
    - send_message: Send a message to another instance by peer ID or name.\n\
    - set_summary: Set a 1-2 sentence summary of what you're working on; surfaced on list_panes / list_peers for other peers.\n\
    - check_messages: Read one bounded inbox page, assemble all pages, then explicitly acknowledge \
    complete receipt; retry the same cursor without ack after truncation.\n\n\
    Pane control tools (all scoped to the current renga tab, except new_tab which is the one \
    cross-tab tool):\n\
    - list_panes: Inspect all panes in the current tab, including geometry and the focus flag.\n\
    - spawn_pane: Split an existing pane to create a new one. Optionally queues a startup command \
    for asynchronous execution; process startup is not confirmed when the tool returns. With no \
    explicit `command`, role `claude` queues the peer-enabled Claude command. The tool can also \
    assign a stable name, attach a free-form role label shown in the UI and list_panes, or set an \
    explicit working directory via \
    `cwd` (absolute, or relative to the caller pane's cwd). Use `cwd` instead of `cd <dir> && ...` \
    inside `command` so the claude auto-upgrade keeps working.\n\
    - spawn_claude_pane: Higher-level convenience when the target process is Claude Code. Takes \
    structured `permission_mode` / `model` / `args[]` fields instead of a free-form command \
    string and queues the peer-enabled Claude startup command. Process startup is asynchronous and \
    not confirmed when the tool returns; allow startup time, then use inspect_pane to verify. Prefer \
    this over `spawn_pane(command=\"claude ...\")` \
    for orchestrator flows — keeps Claude launch policy in renga instead of in every prompt.\n\
    - spawn_codex_pane: Higher-level convenience when the target process is Codex. Takes \
    structured `args[]` instead of a free-form command string and queues a plain `codex` startup \
    command. Process startup is asynchronous and not confirmed when the tool returns; allow startup \
    time, then use inspect_pane to verify. Prefer this over `spawn_pane(command=\"codex ...\")` so \
    orchestrator prompts do not have to synthesize shell-quoted Codex commands.\n\
    - close_pane: Close a pane by id or name. Refuses when it's the last pane of the last tab.\n\
    - focus_pane: Move keyboard focus to another pane in the same tab.\n\
    - new_tab: Open a brand-new tab with a fresh pane and switch focus to it. Unlike the other \
    pane-control tools, this reaches outside the current tab. Accepts the same `cwd` option \
    as spawn_pane for setting the new pane's working directory. Any effective startup command is \
    queued for asynchronous execution, and process startup is not confirmed when the tool returns. \
    With no explicit `command`, role `claude` queues the peer-enabled Claude command for automatic \
    execution; an explicit `command` takes precedence.\n\
    - inspect_pane: Snapshot the visible screen of a pane so you can detect interactive \
    prompts, banners, or mode indicators in another pane without asking it. Returns plain \
    text by default; pass format=\"grid\" for row-addressable JSON or lines=N to trim to \
    the last N rows.\n\
    - send_keys: Send raw key input (y/n, Shift+Tab, Esc, arrow keys, Ctrl+letters, etc.) to a \
    pane's PTY. Use this to answer interactive prompts or drive a TUI when the target isn't a \
    peer-enabled agent that can read send_message. DISTINCT from send_message, which delivers \
    logical peer messages rather than PTY bytes.\n\n\
    Event monitoring:\n\
    - poll_events: Long-poll for pane lifecycle events (pane_started, pane_exited, \
    events_dropped). First call (no `since`) starts at \"right now\" — no historical replay. \
    Each response includes a `next_since` cursor to pass back on the next call. Optional \
    `types` filter narrows returned events without losing the cursor advance, but it does \
    not extend the long-poll: a non-matching event still returns early with events=[] \
    and an advanced cursor, so the caller should re-poll for the next window.\n\n\
    Queuing Claude Code startup: prefer spawn_claude_pane — it takes structured \
    `permission_mode` / `model` / `args[]` fields, always enables the peer channel, and keeps \
    launch policy in renga so orchestrator prompts never have to synthesize shell-quoted command \
    strings. For arbitrary shell commands (non-Claude), use spawn_pane / new_tab. When the \
    requested command starts with a bare `claude` invocation, the MCP injects the peer-enabling \
    flags after `claude` while preserving all caller-provided trailing arguments \
    (`claude --dangerously-load-development-channels server:renga-peers \
    --permission-mode bypassPermissions ...`), but \
    spawn_claude_pane is the recommended API for agent harnesses. For Codex startup commands, prefer \
    spawn_codex_pane once `renga mcp install --client codex` has been run for that user.\n\n\
    IMPORTANT about pane control: these tools affect the user's live layout. Use them with \
    restraint — don't close or focus panes you don't own unless the user asked you to. When in \
    doubt, ask first."
    )
}

fn tools_spec() -> Value {
    json!([
        {
            "name": "list_peers",
            "description": "List other peer-enabled panes in the same renga tab. Each peer includes id, optional name / role, cwd, pending_peer_messages (undelivered peer messages / nudges owned by that pane), and when known the client kind and whether it receives messages via push or polling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["machine", "directory", "repo"],
                        "description": "Accepted for wire-compat with claude-peers-mcp. renga always treats scope as the current tab; this parameter is ignored."
                    }
                }
            }
        },
        {
            "name": "send_message",
            "description": "Send a message to another pane in the same renga tab. Claude recipients see it as a <channel source=\"renga-peers\"> tag; Codex panes receive a pane-local nudge from renga and then read the actual queued message via `check_messages`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to_id":   { "type": "string", "description": "Recipient pane id (from list_peers) or stable name." },
                    "message": { "type": "string", "description": "Text to deliver." }
                },
                "required": ["to_id", "message"]
            }
        },
        {
            "name": "set_summary",
            "description": "Set a 1-2 sentence summary of what this pane is currently working on. Surfaced on every PaneInfo / PeerInfo entry returned by list_panes and list_peers so other peer agents can see it. An empty string clears the summary. Max 256 chars (rejected with [summary_too_long]).",
            "inputSchema": {
                "type": "object",
                "properties": { "summary": { "type": "string" } },
                "required": ["summary"]
            }
        },
        {
            "name": "check_messages",
            "description": "Read one bounded page from the queued peer inbox without removing it. Read structuredContent.messages[0].body when a complete message fits; otherwise append structuredContent.delivery.body_chunk and call check_messages again with the returned message_id / next_offset_bytes. Assemble the complete body before acting on it, then acknowledge receipt with ack {message_id, token}; only that explicit ack removes the message. The ack response is confirmation only and never contains the next message body. If pending_after is greater than zero, call check_messages({}) again immediately to read it; renga may also send a follow-up nudge, but do not wait for it. If a response is truncated, retry the same cursor without ack and optionally lower max_response_bytes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_response_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": CHECK_MESSAGES_MAX_RESPONSE_BYTES,
                        "default": CHECK_MESSAGES_DEFAULT_RESPONSE_BYTES,
                        "description": "Maximum serialized JSON-RPC response size. Defaults to 4096 as a transport page size, not a Codex context threshold. Lower it and retry the same cursor without ack if the client truncates the response."
                    },
                    "message_id": {
                        "type": "string",
                        "description": "Opaque id returned for the current FIFO head. Echo it with next_offset_bytes to fetch another page."
                    },
                    "offset_bytes": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "UTF-8 byte offset returned as next_offset_bytes by the preceding page. Defaults to 0."
                    },
                    "ack": {
                        "type": "object",
                        "description": "Confirms complete receipt, not completion of the requested work. Removes only the matching FIFO head and returns confirmation without the next message body.",
                        "properties": {
                            "message_id": { "type": "string" },
                            "token": { "type": "string" }
                        },
                        "required": ["message_id", "token"]
                    }
                }
            }
        },
        {
            "name": "list_panes",
            "description": "List every pane in the current renga tab, with stable id, optional name / role, focused flag, terminal geometry, cwd, pending_peer_messages (undelivered peer messages / nudges owned by that pane), and when known the peer client kind / receive mode. Complements list_peers (which only returns other panes and hides geometry).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "spawn_pane",
            "description": "Split a pane to create a new one in the same renga tab. Returns the new pane's numeric id so you can address it from later tool calls. Refuses if the target is already at minimum size or the tab has hit its pane cap. renga queues any effective startup command for asynchronous execution; the process has not been confirmed started when this tool returns, so allow startup time and use `inspect_pane` to verify. When `command` is omitted and `role` is exactly `claude`, renga queues the peer-enabled Claude startup command for automatic execution; an explicit `command` takes precedence. Claude may take 90–150 s to start.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["vertical", "horizontal"],
                        "description": "`vertical` splits side-by-side (new pane to the right); `horizontal` splits top/bottom (new pane on the bottom)."
                    },
                    "target": {
                        "type": "string",
                        "description": "Pane to split. Numeric id (from list_panes), stable name, or the literal 'focused'. Defaults to 'focused' when omitted. All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead."
                    },
                    "command": {
                        "type": "string",
                        "description": "Optional shell command to run in the new pane once the shell is ready (e.g. 'claude', 'cargo test'). A bare `claude` (or `claude <args>`) is auto-upgraded to the Alt+P form so the new instance joins the renga-peers network — you don't need to pass the --dangerously-load-development-channels flag yourself. If you pass that flag explicitly, it is left alone."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane so it can be addressed by name later."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label (e.g. 'worker', 'leader'). Shown in the UI and in list_panes output. When `command` is omitted and `role` is exactly `claude`, renga queues the peer-enabled Claude startup command for automatic execution; an explicit `command` takes precedence."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. When omitted, the new pane inherits the target pane's cwd (prior behavior). Use this instead of embedding `cd <path> && ...` in `command` — keeps the shell-quoting and the claude auto-upgrade intact."
                    }
                },
                "required": ["direction"]
            }
        },
        {
            "name": "spawn_claude_pane",
            "description": "Higher-level convenience over `spawn_pane`: creates a split pane and queues a Claude Code startup command with the renga-peers channel enabled by construction, so the orchestrating caller never has to synthesize the `--dangerously-load-development-channels server:renga-peers` flag. Process startup is asynchronous and has not been confirmed when this tool returns; allow startup time, then use `inspect_pane` to verify (Claude may take 90–150 s). Structured fields (`permission_mode`, `model`) are rendered into the final command exactly once; extra `args[]` are appended after them. renga applies POSIX-style shell quoting for values that contain whitespace or shell metacharacters, targeting bash / zsh / Git Bash — values containing single quotes may not round-trip cleanly on PowerShell-fallback Windows hosts, so prefer alphanumerics + `_-./:@+%=` in structured values. Conflicting overrides inside `args[]` (--dangerously-load-development-channels / --permission-mode / --model) are rejected with `invalid-params` — use the structured fields instead. Pane creation semantics (split refusal, cwd validation, name / role attachment) match `spawn_pane`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["vertical", "horizontal"],
                        "description": "`vertical` splits side-by-side (new pane to the right); `horizontal` splits top/bottom (new pane on the bottom)."
                    },
                    "target": {
                        "type": "string",
                        "description": "Pane to split. Numeric id, stable name, or the literal 'focused'. Defaults to 'focused' when omitted."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane so it can be addressed by name later."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label (e.g. 'worker', 'foreman', 'curator'). Shown in the UI and in list_panes output."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. Same semantics as `spawn_pane`'s cwd."
                    },
                    "permission_mode": {
                        "type": "string",
                        "description": "Rendered into the launch command as `--permission-mode <value>`. Typical values: 'default', 'acceptEdits', 'bypassPermissions', 'plan'. Not pre-validated against a fixed enum so new Claude permission modes work without a renga release."
                    },
                    "model": {
                        "type": "string",
                        "description": "Rendered into the launch command as `--model <value>` (e.g. 'sonnet', 'opus', or a fully-qualified model id)."
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Additional Claude CLI args appended after the structured fields. Must NOT contain --dangerously-load-development-channels, --permission-mode, or --model — pass those via the structured fields instead, or the call is rejected with invalid-params."
                    }
                },
                "required": ["direction"]
            }
        },
        {
            "name": "spawn_codex_pane",
            "description": "Higher-level convenience over `spawn_pane`: creates a split pane and queues a Codex startup command without the orchestrating caller having to synthesize a shell-quoted `codex ...` command string. Process startup is asynchronous and has not been confirmed when this tool returns; allow startup time, then use `inspect_pane` to verify (Codex may take 90–150 s). This helper assumes the user has already run `renga mcp install --client codex`; that registration injects the `RENGA_PEER_CLIENT_KIND=codex` env into Codex's MCP server subprocess, so a plain `codex` launch is enough for the new pane to register as a pull-based peer. Extra `args[]` are appended after the `codex` token using the same POSIX-style shell quoting as spawn_claude_pane. Pane creation semantics (split refusal, cwd validation, name / role attachment) match `spawn_pane`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["vertical", "horizontal"],
                        "description": "`vertical` splits side-by-side (new pane to the right); `horizontal` splits top/bottom (new pane on the bottom)."
                    },
                    "target": {
                        "type": "string",
                        "description": "Pane to split. Numeric id, stable name, or the literal 'focused'. Defaults to 'focused' when omitted."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane so it can be addressed by name later."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label (e.g. 'worker', 'reviewer', 'curator'). Shown in the UI and in list_panes output."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. Same semantics as `spawn_pane`'s cwd."
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Additional Codex CLI args appended after the `codex` token. renga owns shell quoting for each item, so callers should pass one logical token per array entry."
                    }
                },
                "required": ["direction"]
            }
        },
        {
            "name": "close_pane",
            "description": "Close a pane in the current renga tab, terminating its process. Fails with code 'last_pane' when the target is the last pane of the only remaining tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to close. Numeric id (from list_panes), stable name, or the literal 'focused'. All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "focus_pane",
            "description": "Move keyboard focus to another pane in the current renga tab. The focused pane is what the user's keystrokes go to, so use sparingly — yanking focus away from the user is disruptive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to focus. Numeric id (from list_panes), stable name, or the literal 'focused' (a no-op, kept for symmetry with the other pane tools). All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "new_tab",
            "description": "Create a new renga tab with a fresh single pane. Focus switches to the new tab. Returns the new pane's numeric id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Optional shell command to queue in the new pane. Process start is asynchronous and not yet confirmed when new_tab returns; allow startup time, then use inspect_pane to verify. A bare `claude` (or `claude <args>`) is auto-upgraded to the Alt+P peer-enabled form so the new instance joins the renga-peers network. If you pass --dangerously-load-development-channels explicitly, it is left alone."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane."
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional tab label. Defaults to a label derived from the cwd."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label attached to the new pane. When command is omitted and role is exactly `claude`, renga queues the peer-enabled Claude startup command; an explicit command takes precedence. Process start is not confirmed by the response."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new tab's pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. When omitted, the renga server's current cwd is used."
                    }
                }
            }
        },
        {
            "name": "inspect_pane",
            "description": "Snapshot the visible screen of a pane in the current renga tab. Returns the rendered contents so you can detect interactive prompts (e.g. y/n confirmations), error banners, or mode indicators in another pane without asking its Claude. The `lines` option trims the response to the bottom N rows (blank rows preserved, useful for anchoring on a status bar). `format=\"grid\"` switches the text block to JSON with one row object per line; the full structured payload is always available in `structuredContent`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to inspect. Numeric id (from list_panes), stable name, or the literal 'focused'. All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead."
                    },
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional — trim the response to the bottom N rows of the screen grid. Blank rows are preserved. Omit for the full visible screen."
                    },
                    "include_cursor": {
                        "type": "boolean",
                        "description": "When true, the payload includes a `cursor` object ({visible, row, col}). Defaults to false."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["text", "grid"],
                        "description": "'text' (default) returns the plain rendered screen as the content text. 'grid' returns a JSON blob with one object per row. `structuredContent` is always populated with the full payload regardless of this choice."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "send_keys",
            "description": "Send raw keystrokes to a pane's PTY — useful for answering interactive prompts (y/n), toggling Claude Code's permission mode (Shift+Tab), or driving any TUI that expects real key events instead of logical messages. Named special keys are translated to terminal escape sequences server-side; `text` passes through verbatim; the two can be combined. NOTE: this is NOT send_message. send_message delivers a logical peer message to another Claude via a channel notification; send_keys writes bytes into a PTY and is visible to whatever application is running in that pane.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to send to. Numeric id, stable name, or 'focused'. All-digit strings are always ids."
                    },
                    "text": {
                        "type": "string",
                        "description": "Literal text sent before any named keys. Use this for anything that doesn't need special-key translation (e.g. 'y', 'npm install')."
                    },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ordered list of named special keys appended after `text`. Supported vocabulary: Enter / Return, Tab, Shift+Tab (a.k.a. BackTab), Esc / Escape, Backspace, Delete / Del, Up / Down / Left / Right, Home, End, PageUp, PageDown, Space, Ctrl+<letter> where <letter> is A-Z. Unknown names return an -32602 invalid-params error."
                    },
                    "enter": {
                        "type": "boolean",
                        "description": "Convenience — append an Enter after `text` and `keys`. Equivalent to adding 'Enter' to the end of `keys`."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "set_pane_identity",
            "description": "Rename or (re)assign the stable `name` and/or `role` of an existing pane in the current tab. Use this to recover from sessions launched without the intended layout (e.g. when the secretary pane was spawned without an `id`, so peers can't address it as `to_id=\"secretary\"`). Both fields use three-state semantics: omit the key to keep the current value, pass `null` to clear it, or pass a string to set it. Validation: name cannot be empty, all-digits, or collide with another pane in this tab; allowed characters are [A-Za-z0-9_-]. Role has no uniqueness constraint. Returns the updated pane record so callers can confirm without a separate list round-trip.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to update. Numeric id (from list_panes), stable name, or the literal 'focused' (default). All-digit strings are always ids."
                    },
                    "name": {
                        "type": ["string", "null"],
                        "description": "New name, or null to clear. Omit to leave unchanged."
                    },
                    "role": {
                        "type": ["string", "null"],
                        "description": "New role label, or null to clear. Omit to leave unchanged."
                    }
                }
            }
        },
        {
            "name": "poll_events",
            "description": "Long-poll for pane lifecycle events (pane_started, pane_exited, events_dropped, and any forward-compatible variants). Returns events accumulated since the given cursor; if none are buffered, blocks up to `timeout_ms` for the next one. The first call (omit `since`) starts at \"right now\" — no historical replay, matching `renga events --timeout` semantics. Each response body is a JSON object with `next_since` (an opaque cursor string to pass back) and `events` (an array of event objects in renga's wire format).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": {
                        "type": "string",
                        "description": "Cursor from a prior response's `next_since`. Omit on the first call to start at the present."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Maximum milliseconds to block when no event is immediately available. Default 2000; clamped to a 30000 ms maximum. Pass 0 for a non-blocking drain.",
                        "minimum": 0
                    },
                    "types": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional filter — only return events whose `type` field is in this list. The cursor still advances past filtered-out events so they won't reappear. Note: the filter narrows returned results but does not extend the long-poll; if a non-matching event arrives during the wait, `poll_events` returns early with `events: []` and an advanced cursor, and the caller should re-poll for the next window."
                    }
                }
            }
        }
    ])
}

fn handle_initialize(id: &Value, params: &Value, ctx: &PeerCtx) -> Value {
    let client_protocol = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("2025-06-18");
    let experimental = match ctx.client_kind {
        PeerClientKind::Claude => json!({ "claude/channel": {} }),
        PeerClientKind::Codex => json!({}),
    };
    ok_response(
        id,
        json!({
            "protocolVersion": client_protocol,
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            "capabilities": {
                "experimental": experimental,
                "tools": {}
            },
            "instructions": instructions_blob(ctx.client_kind)
        }),
    )
}

fn handle_tools_list(id: &Value) -> Value {
    ok_response(id, json!({ "tools": tools_spec() }))
}

fn handle_list_peers(id: &Value, ctx: &PeerCtx) -> Value {
    let (pane_id, endpoint) = match &ctx.mode {
        Mode::Connected { pane_id, endpoint } => (*pane_id, endpoint),
        Mode::Detached { reason } => {
            return ok_response(
                id,
                tool_text_result(&format!(
                    "(no peers — renga not reachable from this peer client: {reason})"
                )),
            );
        }
    };
    match client::send_request(endpoint, &Request::PeerList { from_pane: pane_id }) {
        Ok(Response::Ok { data }) => match serde_json::from_value::<Vec<PeerInfo>>(data) {
            Ok(peers) => ok_response(id, tool_text_result(&format_peer_list(&peers))),
            Err(e) => err_response(id, -32603, &format!("decode peer list: {e}")),
        },
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused list_peers: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn format_peer_list(peers: &[PeerInfo]) -> String {
    if peers.is_empty() {
        return "No peers in this tab.".to_string();
    }
    let mut out = String::from("Peers in this tab:\n\n");
    for p in peers {
        out.push_str(&format!("- id={}", p.id));
        out.push_str(&format!(
            " pending_peer_messages={}",
            p.pending_peer_messages
        ));
        if let Some(name) = &p.name {
            out.push_str(&format!(" name={name}"));
        }
        if let Some(role) = &p.role {
            out.push_str(&format!(" role={role}"));
        }
        if let Some(kind) = p.kind {
            out.push_str(&format!(" kind={}", kind_label(kind)));
        }
        if let Some(mode) = p.receive_mode {
            out.push_str(&format!(" receive={}", receive_mode_label(mode)));
        }
        if let Some(cwd) = &p.cwd {
            out.push_str(&format!("\n  cwd: {cwd}"));
        }
        out.push('\n');
    }
    out
}

fn kind_label(kind: PeerClientKind) -> &'static str {
    match kind {
        PeerClientKind::Claude => "claude",
        PeerClientKind::Codex => "codex",
    }
}

fn receive_mode_label(mode: ipc::PeerReceiveMode) -> &'static str {
    match mode {
        ipc::PeerReceiveMode::Push => "push",
        ipc::PeerReceiveMode::Pull => "pull",
    }
}

fn peer_send_result_text(to_id: &str, data: &Value) -> String {
    match data.get("delivery").and_then(Value::as_str) {
        Some("delivered") => format!("Delivered to {to_id}."),
        Some("queued") => format!("Queued for {to_id} (peer client not registered yet)."),
        Some("pending_user_confirmation") => {
            format!("Pending user confirmation for {to_id}.")
        }
        Some("undeliverable") => format!(
            "Not delivered to {to_id}: no such pane in this tab (pane ids and names are tab-scoped)."
        ),
        // Older renga servers return a successful response without the
        // delivery field and may have dropped an unregistered peer send.
        // Unknown future values are equally unverified, so only the explicit
        // delivered value may produce a Delivered claim.
        _ => format!(
            "Message sent to {to_id}; delivery state unconfirmed (renga server may predate queued delivery)."
        ),
    }
}

fn handle_send_message(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let to_id = args.get("to_id").and_then(|v| v.as_str()).unwrap_or("");
    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
    if to_id.is_empty() {
        return err_response(id, -32602, "send_message requires a non-empty to_id");
    }
    let (pane_id, endpoint) = match &ctx.mode {
        Mode::Connected { pane_id, endpoint } => (*pane_id, endpoint),
        Mode::Detached { reason } => {
            return ok_response(
                id,
                tool_text_result(&format!(
                    "(message dropped — renga not reachable: {reason})"
                )),
            );
        }
    };
    let target = match to_id.parse::<usize>() {
        Ok(n) => PaneRef::Id(n),
        Err(_) => PaneRef::Name(to_id.to_string()),
    };
    match client::send_request(
        endpoint,
        &Request::PeerSend {
            from_pane: pane_id,
            target,
            body: message.to_string(),
        },
    ) {
        Ok(Response::Ok { data }) => {
            ok_response(id, tool_text_result(&peer_send_result_text(to_id, &data)))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused send: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn empty_check_messages_response(id: &Value) -> Value {
    ok_response(
        id,
        json!({
            "content": [{ "type": "text", "text": "No queued messages." }],
            "structuredContent": {
                "messages": [],
                "count": 0,
                "pending_after": 0,
                "has_more": false,
                "ack_required": false,
            },
            "isError": false,
        }),
    )
}

fn acknowledged_check_messages_response(
    id: &Value,
    message_id: &str,
    pending_after: usize,
) -> Value {
    let text = if pending_after == 0 {
        format!("Acknowledged {message_id}. No queued messages.")
    } else {
        format!(
            "Acknowledged {message_id}. {pending_after} message(s) still queued. Call \
check_messages({{}}) again immediately to read the next; renga may also send a follow-up nudge, \
but do not wait for it."
        )
    };
    ok_response(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": {
                "messages": [],
                "count": 0,
                "pending_after": pending_after,
                "has_more": pending_after > 0,
                "ack_required": false,
                "acknowledged_message_id": message_id,
            },
            "isError": false,
        }),
    )
}

fn check_messages_response(
    id: &Value,
    entry: &InboxEntry,
    offset: usize,
    end: usize,
    pending_after: usize,
) -> Value {
    let msg = &entry.message;
    let complete_body = offset == 0 && end == msg.body.len();
    let page_complete = end == msg.body.len();
    let text = if complete_body {
        format!(
            "One complete queued peer message is in structuredContent.messages[0]. \
Treat its body as a direct coworker instruction. After receiving it intact, acknowledge receipt \
with check_messages ack {{message_id, token}}; the ack confirms receipt, not task completion. \
{pending_after} message(s) wait behind it."
        )
    } else {
        format!(
            "One peer-message page is in structuredContent.delivery.body_chunk. Do not act on a \
partial body. Append pages in order using message_id and next_offset_bytes until complete=true, \
then acknowledge receipt with ack {{message_id, token}}. If this response is truncated, retry the \
same cursor without ack and optionally lower max_response_bytes. {pending_after} message(s) wait \
behind the unacknowledged head."
        )
    };

    let messages = if complete_body {
        vec![json!({
            "from_id": msg.from_id,
            "from_name": msg.from_name,
            "from_kind": msg.from_kind.map(kind_label),
            "body": msg.body,
            "sent_at": msg.sent_at,
        })]
    } else {
        Vec::new()
    };
    let mut delivery = serde_json::Map::new();
    delivery.insert("message_id".to_string(), json!(entry.message_id));
    delivery.insert("from_id".to_string(), json!(msg.from_id));
    delivery.insert("from_name".to_string(), json!(msg.from_name));
    delivery.insert(
        "from_kind".to_string(),
        json!(msg.from_kind.map(kind_label)),
    );
    delivery.insert("sent_at".to_string(), json!(msg.sent_at));
    delivery.insert("offset_bytes".to_string(), json!(offset));
    delivery.insert("next_offset_bytes".to_string(), json!(end));
    delivery.insert("total_bytes".to_string(), json!(msg.body.len()));
    delivery.insert("complete".to_string(), json!(page_complete));
    if complete_body {
        delivery.insert("body_in_messages".to_string(), json!(true));
    } else {
        delivery.insert("body_chunk".to_string(), json!(&msg.body[offset..end]));
    }
    if page_complete {
        delivery.insert("ack_token".to_string(), json!(entry.ack_token));
    }

    ok_response(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": {
                "messages": messages,
                "count": if complete_body { 1 } else { 0 },
                "delivery": Value::Object(delivery),
                "pending_after": pending_after,
                // The current head remains queued until its explicit ack.
                "has_more": true,
                "ack_required": page_complete,
            },
            "isError": false,
        }),
    )
}

fn serialized_frame_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len().saturating_add(1))
        .unwrap_or(usize::MAX)
}

fn parse_check_response_budget(args: &Value) -> std::result::Result<usize, String> {
    let Some(raw) = args.get("max_response_bytes") else {
        return Ok(CHECK_MESSAGES_DEFAULT_RESPONSE_BYTES);
    };
    let value = raw
        .as_u64()
        .ok_or_else(|| "max_response_bytes must be a positive integer".to_string())?;
    let value = usize::try_from(value)
        .map_err(|_| "max_response_bytes is too large for this platform".to_string())?;
    if value == 0 || value > CHECK_MESSAGES_MAX_RESPONSE_BYTES {
        return Err(format!(
            "max_response_bytes must be between 1 and {CHECK_MESSAGES_MAX_RESPONSE_BYTES}"
        ));
    }
    Ok(value)
}

fn check_messages_page_response(
    id: &Value,
    entry: &InboxEntry,
    offset: usize,
    pending_after: usize,
    budget: usize,
) -> std::result::Result<Value, String> {
    let body = &entry.message.body;
    if offset > body.len() || !body.is_char_boundary(offset) {
        return Err("offset_bytes is not a UTF-8 character position in this message".to_string());
    }

    if body.len().saturating_sub(offset) <= budget {
        let full = check_messages_response(id, entry, offset, body.len(), pending_after);
        if serialized_frame_len(&full) <= budget {
            return Ok(full);
        }
    }

    let positions: Vec<usize> = body[offset..]
        .char_indices()
        .map(|(relative, _)| offset + relative)
        .take_while(|position| position.saturating_sub(offset) <= budget)
        .chain(std::iter::once(
            offset.saturating_add(budget).min(body.len()),
        ))
        .map(|mut position| {
            while position > offset && !body.is_char_boundary(position) {
                position -= 1;
            }
            position
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut positions = positions;
    positions.sort_unstable();
    let mut low = 0usize;
    let mut high = positions.len();
    while low < high {
        let mid = low + (high - low) / 2;
        let candidate = check_messages_response(id, entry, offset, positions[mid], pending_after);
        if serialized_frame_len(&candidate) <= budget {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low == 0 {
        return Err(format!(
            "max_response_bytes={budget} is too small for the check_messages metadata envelope"
        ));
    }
    let end = positions[low - 1];
    if end == offset && offset < body.len() {
        return Err(format!(
            "max_response_bytes={budget} cannot fit one UTF-8 character plus the check_messages metadata envelope"
        ));
    }
    Ok(check_messages_response(
        id,
        entry,
        offset,
        end,
        pending_after,
    ))
}

struct CheckMessagesHandled {
    response: Value,
    renudge_after_ack: Option<&'static str>,
    consumed_after_ack: Option<&'static str>,
}

impl CheckMessagesHandled {
    fn plain(response: Value) -> Self {
        Self {
            response,
            renudge_after_ack: None,
            consumed_after_ack: None,
        }
    }
}

fn handle_check_messages_inner(id: &Value, args: &Value, ctx: &PeerCtx) -> CheckMessagesHandled {
    let budget = match parse_check_response_budget(args) {
        Ok(value) => value,
        Err(message) => return CheckMessagesHandled::plain(err_response(id, -32602, &message)),
    };
    let has_cursor = args.get("message_id").is_some() || args.get("offset_bytes").is_some();
    if args.get("ack").is_some() && has_cursor {
        return CheckMessagesHandled::plain(err_response(
            id,
            -32602,
            "ack cannot be combined with message_id or offset_bytes",
        ));
    }

    let mut inbox = ctx.inbox.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(ack) = args.get("ack") {
        let Some(ack) = ack.as_object() else {
            return CheckMessagesHandled::plain(err_response(id, -32602, "ack must be an object"));
        };
        let Some(message_id) = ack.get("message_id").and_then(Value::as_str) else {
            return CheckMessagesHandled::plain(err_response(
                id,
                -32602,
                "ack.message_id must be a string",
            ));
        };
        let Some(token) = ack.get("token").and_then(Value::as_str) else {
            return CheckMessagesHandled::plain(err_response(
                id,
                -32602,
                "ack.token must be a string",
            ));
        };
        let Some(head) = inbox.messages.front() else {
            return CheckMessagesHandled::plain(err_response(
                id,
                -32602,
                "ack does not match a queued message",
            ));
        };
        if head.message_id != message_id || head.ack_token != token {
            return CheckMessagesHandled::plain(err_response(
                id,
                -32602,
                "ack does not match the queued FIFO head",
            ));
        }
        let delivery_id = head.message.delivery_id;
        inbox.messages.pop_front();
        let pending_after = inbox.messages.len();
        let next = inbox.messages.front().map(|entry| entry.message.clone());
        drop(inbox);
        let renudge_after_ack = match next.as_ref() {
            Some(next) => request_codex_renudge_after_ack(ctx, pending_after, next),
            None => "skipped_none_pending",
        };
        if let Some(delivery_id) = delivery_id {
            retain_unreported_consumed(ctx, delivery_id);
        }
        let consumed_after_ack = request_peer_inbox_consumed(ctx, delivery_id);
        return CheckMessagesHandled {
            response: acknowledged_check_messages_response(id, message_id, pending_after),
            renudge_after_ack: Some(renudge_after_ack),
            consumed_after_ack: Some(consumed_after_ack),
        };
    }

    let Some(entry) = inbox.messages.front() else {
        return CheckMessagesHandled::plain(empty_check_messages_response(id));
    };
    let requested_id = args.get("message_id").and_then(Value::as_str);
    if args.get("message_id").is_some() && requested_id.is_none() {
        return CheckMessagesHandled::plain(err_response(
            id,
            -32602,
            "message_id must be a string",
        ));
    }
    if let Some(requested_id) = requested_id {
        if requested_id != entry.message_id {
            return CheckMessagesHandled::plain(err_response(
                id,
                -32602,
                "message_id does not match the queued FIFO head",
            ));
        }
    }
    let offset = match args.get("offset_bytes") {
        Some(value) => match value.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(value) => value,
            None => {
                return CheckMessagesHandled::plain(err_response(
                    id,
                    -32602,
                    "offset_bytes must be a non-negative integer",
                ))
            }
        },
        None => 0,
    };
    if offset != 0 && requested_id.is_none() {
        return CheckMessagesHandled::plain(err_response(
            id,
            -32602,
            "a non-zero offset_bytes requires the returned message_id",
        ));
    }
    let pending_after = inbox.messages.len().saturating_sub(1);
    CheckMessagesHandled::plain(
        match check_messages_page_response(id, entry, offset, pending_after, budget) {
            Ok(response) => response,
            Err(message) => err_response(id, -32602, &message),
        },
    )
}

fn handle_check_messages(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let Some(path) = ctx.debug_log_path.as_deref() else {
        return handle_check_messages_inner(id, args, ctx).response;
    };
    let ack = args.get("ack");
    let has_cursor = args.get("message_id").is_some() || args.get("offset_bytes").is_some();
    let (inbox_len_before, ack_would_be_accepted) = {
        let inbox = ctx.inbox.lock().unwrap_or_else(|p| p.into_inner());
        let ack_would_be_accepted = parse_check_response_budget(args).is_ok()
            && !has_cursor
            && ack
                .and_then(Value::as_object)
                .zip(inbox.messages.front())
                .is_some_and(|(ack, head)| {
                    ack.get("message_id").and_then(Value::as_str) == Some(&head.message_id)
                        && ack.get("token").and_then(Value::as_str) == Some(&head.ack_token)
                });
        (inbox.messages.len(), ack_would_be_accepted)
    };
    let CheckMessagesHandled {
        response,
        renudge_after_ack,
        consumed_after_ack,
    } = handle_check_messages_inner(id, args, ctx);
    let inbox_len_after = ctx
        .inbox
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messages
        .len();

    let call_shape = if args.get("ack").is_some() {
        "ack"
    } else if args.get("message_id").is_some() || args.get("offset_bytes").is_some() {
        "cursor"
    } else {
        "empty"
    };
    let ack_result = if ack.is_none() {
        "none".to_string()
    } else if ack_would_be_accepted {
        "accepted".to_string()
    } else {
        let reason = response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("unknown reason");
        format!("rejected:{reason}")
    };
    let structured = response.pointer("/result/structuredContent");
    let delivery = structured.and_then(|value| value.get("delivery"));
    let error_reason = response.pointer("/error/message").and_then(Value::as_str);
    let body_len = delivery
        .and_then(|value| value.get("body_chunk"))
        .and_then(Value::as_str)
        .or_else(|| {
            structured
                .and_then(|value| value.pointer("/messages/0/body"))
                .and_then(Value::as_str)
        })
        .map(str::len);
    append_peer_debug_record(
        path,
        peer_ctx_pane_id(ctx),
        json!({
            "action": "check_messages",
            "call_shape": call_shape,
            "args": {
                "message_id": args.get("message_id").and_then(Value::as_str),
                "offset_bytes": args.get("offset_bytes").and_then(Value::as_u64),
                "ack": ack.map(|value| json!({
                    "message_id": value.get("message_id").and_then(Value::as_str),
                    "token_present": value.get("token").is_some(),
                })),
            },
            "ack_result": ack_result,
            "renudge_after_ack": renudge_after_ack,
            "consumed_after_ack": consumed_after_ack,
            "error_reason": error_reason,
            "response": {
                "head_message_id": delivery.and_then(|value| value.get("message_id")).and_then(Value::as_str),
                "count": structured.and_then(|value| value.get("count")).and_then(Value::as_u64),
                "pending_after": structured.and_then(|value| value.get("pending_after")).and_then(Value::as_u64),
                "has_more": structured.and_then(|value| value.get("has_more")).and_then(Value::as_bool),
                "ack_token_present": delivery.and_then(|value| value.get("ack_token")).is_some(),
                "page_offset": delivery.and_then(|value| value.get("offset_bytes")).and_then(Value::as_u64),
                "body_len": body_len,
            },
            "inbox_len_before": inbox_len_before,
            "inbox_len_after": inbox_len_after,
        }),
    );
    response
}

fn peer_ctx_pane_id(ctx: &PeerCtx) -> Option<usize> {
    match &ctx.mode {
        Mode::Connected { pane_id, .. } => Some(*pane_id),
        Mode::Detached { .. } => None,
    }
}

fn fmt_code(message: &str, code: &Option<String>) -> String {
    match code {
        Some(c) => format!("[{c}] {message}"),
        None => message.to_string(),
    }
}

fn handle_tools_call(id: &Value, params: &Value, ctx: &PeerCtx) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("tools/call missing 'name'"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    Ok(match name {
        "list_peers" => handle_list_peers(id, ctx),
        "send_message" => handle_send_message(id, &args, ctx),
        "set_summary" => handle_set_summary(id, &args, ctx),
        "check_messages" => handle_check_messages(id, &args, ctx),
        "list_panes" => handle_list_panes(id, ctx),
        "spawn_pane" => handle_spawn_pane(id, &args, ctx),
        "spawn_claude_pane" => handle_spawn_claude_pane(id, &args, ctx),
        "spawn_codex_pane" => handle_spawn_codex_pane(id, &args, ctx),
        "close_pane" => handle_close_pane(id, &args, ctx),
        "focus_pane" => handle_focus_pane(id, &args, ctx),
        "new_tab" => handle_new_tab(id, &args, ctx),
        "inspect_pane" => handle_inspect_pane(id, &args, ctx),
        "send_keys" => handle_send_keys(id, &args, ctx),
        "poll_events" => handle_poll_events(id, &args, ctx),
        "set_pane_identity" => handle_set_pane_identity(id, &args, ctx),
        other => err_response(id, -32601, &format!("unknown tool: {other}")),
    })
}

// ── pane control handlers ────────────────────────────────────

/// Resolve a tool `target` argument string into a [`PaneRef`].
///
/// Resolution order (first match wins):
/// 1. `None`, empty, whitespace-only, or `"focused"` (case-insensitive)
///    → `PaneRef::Focused`.
/// 2. Parses cleanly as `usize` → `PaneRef::Id(n)`.
/// 3. Otherwise → `PaneRef::Name(s)` (trimmed).
///
/// Edge cases folded into step 3 on purpose: negative-sign strings
/// like `"-1"` and digit strings that overflow `usize` both resolve
/// to `Name`. (Rust's `usize::from_str` accepts a leading `+`, so
/// `"+3"` still parses as `Id(3)` — a quirk inherited from the
/// stdlib, not a renga decision.) renga pane ids live in a small
/// fixed range (capped by `MAX_PANES`), so an overflow-sized "id"
/// can't refer to a real pane either way — letting the server reply
/// with `pane_not_found` on a bogus `Name` is indistinguishable from
/// erroring on `Id`, and keeps `parse_target` infallible.
fn parse_target(raw: Option<&str>) -> PaneRef {
    let Some(s) = raw else {
        return PaneRef::Focused;
    };
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("focused") {
        return PaneRef::Focused;
    }
    match trimmed.parse::<usize>() {
        Ok(n) => PaneRef::Id(n),
        Err(_) => PaneRef::Name(trimmed.to_string()),
    }
}

fn parse_direction(raw: Option<&str>) -> std::result::Result<Direction, String> {
    match raw.map(str::trim) {
        Some("vertical") => Ok(Direction::Vertical),
        Some("horizontal") => Ok(Direction::Horizontal),
        Some(other) => Err(format!(
            "invalid direction {other:?}; expected 'vertical' or 'horizontal'"
        )),
        None => Err("direction is required ('vertical' or 'horizontal')".to_string()),
    }
}

/// Optional string-valued argument extractor. Empty strings map to None
/// so Claude can send `{"command": ""}` without accidentally shoving an
/// empty command line into the new pane.
fn opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Upgrade a bare `claude …` command to the peer-enabled invocation
/// that Alt+P types into a pane. When the caller asks to spawn Claude
/// Code without the `--dangerously-load-development-channels
/// server:renga-peers` flag, the new instance can't see the peer
/// network, which silently defeats half the reason renga wraps it.
/// Injecting the flag at this seam keeps the MCP as a "launch Claude
/// and have it join the network" affordance without making the LLM
/// remember the exact incantation.
///
/// Rules:
/// - If the command already contains
///   `--dangerously-load-development-channels`, leave it alone — the
///   caller knew what they wanted.
/// - Match only when the first whitespace-delimited token is exactly
///   `claude`. `claude-mobile`, `claudex`, `./claude`, or `cargo run
///   -- claude` all fall through untouched so we never rewrite an
///   unrelated command by accident.
/// - Preserve the caller's trailing arguments: `"claude --resume"`
///   becomes `"claude --dangerously-load-development-channels
///   server:renga-peers --permission-mode bypassPermissions --resume"`.
pub(crate) fn upgrade_claude_command(cmd: &str) -> String {
    if cmd.contains("--dangerously-load-development-channels") {
        return cmd.to_string();
    }
    let trimmed = cmd.trim_start();
    let leading_ws_len = cmd.len() - trimmed.len();
    let Some(rest) = trimmed.strip_prefix("claude") else {
        return cmd.to_string();
    };
    // Reject `claudex`, `claude-mobile`, etc. — the next char after
    // the literal token `claude` must be whitespace or end-of-string.
    if !rest.is_empty() && !rest.starts_with(|c: char| c.is_whitespace()) {
        return cmd.to_string();
    }
    let leading = &cmd[..leading_ws_len];
    format!("{leading}{CLAUDE_PEER_LAUNCH_CMD}{rest}")
}

/// Require `Mode::Connected`, otherwise respond with a user-visible
/// "renga unreachable" text result (not a JSON-RPC error, so Claude
/// surfaces the explanation to the user instead of treating the tool
/// as broken).
fn require_connected<'a>(
    ctx: &'a PeerCtx,
    id: &Value,
    action: &str,
) -> std::result::Result<(usize, &'a EndpointName), Value> {
    match &ctx.mode {
        Mode::Connected { pane_id, endpoint } => Ok((*pane_id, endpoint)),
        Mode::Detached { reason } => Err(ok_response(
            id,
            tool_text_result(&format!(
                "(cannot {action} — renga not reachable: {reason})"
            )),
        )),
    }
}

fn handle_list_panes(id: &Value, ctx: &PeerCtx) -> Value {
    let (_caller_pane, endpoint) = match require_connected(ctx, id, "list panes") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request(endpoint, &Request::List) {
        Ok(Response::Ok { data }) => match serde_json::from_value::<Vec<PaneInfo>>(data) {
            Ok(panes) => ok_response(id, tool_text_result(&format_pane_list(&panes))),
            Err(e) => err_response(id, -32603, &format!("decode pane list: {e}")),
        },
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused list_panes: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn format_pane_list(panes: &[PaneInfo]) -> String {
    if panes.is_empty() {
        return "No panes in this tab.".to_string();
    }
    let mut out = String::from("Panes in this tab:\n\n");
    for p in panes {
        out.push_str(&format!("- id={}", p.id));
        out.push_str(&format!(
            " pending_peer_messages={}",
            p.pending_peer_messages
        ));
        if let Some(name) = &p.name {
            out.push_str(&format!(" name={name}"));
        }
        if let Some(role) = &p.role {
            out.push_str(&format!(" role={role}"));
        }
        if p.focused {
            out.push_str(" (focused)");
        }
        out.push_str(&format!(
            "\n  geometry: x={} y={} width={} height={}",
            p.x, p.y, p.width, p.height
        ));
        if let Some(cwd) = &p.cwd {
            out.push_str(&format!("\n  cwd: {cwd}"));
        }
        out.push('\n');
    }
    out
}

/// Resolve a user-supplied `cwd` into what the IPC layer wants: either
/// `None` (use server default) or an absolute-path string. Relative
/// paths are joined onto the caller pane's cwd — the pane the Claude
/// agent is running inside — so Claude's tool calls map to the same
/// cwd its shell would interpret `cd <path>` against. Returns
/// `Err(message)` on unresolvable input (caller pane vanished, etc.);
/// server-side `CWD_INVALID` handles filesystem-level validation.
fn resolve_mcp_cwd(
    endpoint: &EndpointName,
    caller_pane: usize,
    cwd: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    let s = match cwd {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    if s.is_empty() {
        return Ok(None);
    }
    let path = std::path::Path::new(s);
    if path.is_absolute() {
        return Ok(Some(s.to_string()));
    }
    // Relative path — need caller pane's cwd. A single `Request::List`
    // round-trip is cheap and keeps IPC stateless.
    //
    // Snapshot semantics: we resolve against whatever cwd the server
    // knows at this instant, which is driven by OSC 7 updates from the
    // pane's shell. If the shell has `cd`-ed but the update hasn't
    // reached renga yet, the resolution uses the stale value. Callers
    // that need strict ordering should send an absolute path instead
    // of trusting "current" cwd.
    let panes: Vec<PaneInfo> = match client::send_request(endpoint, &Request::List) {
        Ok(Response::Ok { data }) => serde_json::from_value(data)
            .map_err(|e| format!("decode pane list while resolving cwd: {e}"))?,
        Ok(Response::Err { message, code }) => {
            return Err(format!(
                "list panes to resolve cwd: {}",
                fmt_code(&message, &code)
            ));
        }
        Ok(other) => return Err(format!("unexpected renga response: {other:?}")),
        Err(e) => return Err(format!("list panes to resolve cwd: {e}")),
    };
    let base = panes
        .iter()
        .find(|p| p.id == caller_pane)
        .and_then(|p| p.cwd.clone())
        .ok_or_else(|| {
            format!("cannot resolve relative cwd: caller pane {caller_pane} has no known cwd")
        })?;
    let joined = std::path::Path::new(&base).join(path);
    Ok(Some(joined.to_string_lossy().to_string()))
}

fn handle_spawn_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let direction = match parse_direction(args.get("direction").and_then(|v| v.as_str())) {
        Ok(d) => d,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let target = parse_target(args.get("target").and_then(|v| v.as_str()));
    let command = opt_string(args, "command").map(|c| upgrade_claude_command(&c));
    let name = opt_string(args, "name");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");
    let no_startup_command_possible = command.is_none() && role.is_none();

    let (caller_pane, endpoint) = match require_connected(ctx, id, "spawn pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // Resolve a relative cwd against the caller pane's cwd so relative
    // paths in Claude's tool calls behave the way a user would expect
    // when typing them into the pane's shell. Absolute paths are left
    // untouched; `None` is forwarded as-is so the server falls back to
    // its default (target pane's cwd for Split).
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    match client::send_request(
        endpoint,
        &Request::Split {
            target,
            direction,
            command: command.clone(),
            id: name,
            role,
            cwd,
        },
    ) {
        Ok(Response::Ok { data }) => {
            spawn_pane_ok_response(id, &data, command.as_deref(), no_startup_command_possible)
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused spawn_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

pub(crate) fn spawn_pane_ok_response(
    id: &Value,
    data: &Value,
    requested_command: Option<&str>,
    no_startup_command_possible: bool,
) -> Value {
    let new_id = data.get("id").and_then(|v| v.as_u64());
    let pane = created_pane_text(new_id);
    let msg = if no_startup_command_possible
        && !matches!(data.get("startup_command"), Some(Value::String(_)))
    {
        format!("{pane} No startup command requested.")
    } else {
        spawn_startup_message(&pane, None, data, requested_command)
    };
    ok_response(id, tool_text_result(&msg))
}

fn created_pane_text(new_id: Option<u64>) -> String {
    match new_id {
        Some(n) => format!("Created pane id={n}."),
        None => "Created pane (id not reported).".to_string(),
    }
}

fn spawn_startup_message(
    pane: &str,
    product: Option<&str>,
    data: &Value,
    requested_command: Option<&str>,
) -> String {
    match data.get("startup_command") {
        Some(Value::String(command)) => spawn_queued_message(pane, product, command),
        Some(Value::Null) => format!("{pane} No startup command requested."),
        None | Some(_) => spawn_unconfirmed_message(pane, product, requested_command),
    }
}

fn spawn_unconfirmed_message(
    pane: &str,
    product: Option<&str>,
    requested_command: Option<&str>,
) -> String {
    let timing = startup_timing_hint(product);
    let product = product
        .map(|name| format!(" for {name}"))
        .unwrap_or_default();
    let status = format!(
        "Startup command unconfirmed{product} (renga server may predate effective-command reporting; process start not yet confirmed; allow startup time, then use inspect_pane to verify{timing})"
    );
    match requested_command {
        Some(command) => format!("{pane} {status}: {command}"),
        None => format!("{pane} {status}."),
    }
}

fn spawn_queued_message(pane: &str, product: Option<&str>, command: &str) -> String {
    let timing = startup_timing_hint(product);
    let product = product
        .map(|name| format!(" for {name}"))
        .unwrap_or_default();
    format!(
        "{pane} Startup command queued{product} (process start not yet confirmed; allow startup time, then use inspect_pane to verify{timing}): {command}"
    )
}

fn startup_timing_hint(product: Option<&str>) -> &'static str {
    match product {
        Some("Claude") => "; Claude may take 90–150 s",
        Some("Codex") => "; Codex may take 90–150 s",
        _ => "",
    }
}

/// Flags that `spawn_claude_pane` must own — the structured fields
/// render these exactly once, so letting callers also inject them via
/// `args[]` would produce ambiguous command lines (e.g. two
/// `--permission-mode` entries, or a dropped peer-channel flag if a
/// caller overrides it with a narrower value). Rejecting is cleaner
/// than silent de-dup.
const CLAUDE_RESERVED_FLAGS: &[&str] = &[
    "--dangerously-load-development-channels",
    "--permission-mode",
    "--model",
];

/// POSIX-style shell quoting targeted at the shells `renga` actually
/// runs Claude under on the agent-harness path: bash / zsh / sh on
/// Unix, Git Bash on Windows (the default when present).
///
/// A value made of "safe" chars (alphanumerics plus a small punctuation
/// set that never triggers word-splitting / globbing / variable
/// expansion) passes through unquoted so the resulting command line
/// stays readable. Anything else gets wrapped in single quotes with
/// embedded single quotes escaped as `'\''`.
///
/// **Scope limitation:** PowerShell's single-quoted literal does not
/// interpret the `'\''` escape sequence, so a value that mixes single
/// quotes with other characters won't round-trip cleanly when the
/// caller's Windows host lacks Git Bash and falls back to PowerShell.
/// Realistic `spawn_claude_pane` values (permission modes, model ids,
/// flag tokens) never contain single quotes, so the practical exposure
/// is minimal; if callers need PowerShell-safe launches for exotic
/// values they should pass an absolute path or pre-quoted string
/// through `args[]` and understand the shell contract themselves.
///
/// Shared between `build_claude_launch_command` and its tests.
fn shell_quote(value: &str) -> String {
    // Empty string can never be left bare — the shell would drop it
    // entirely, silently losing an argument slot.
    if value.is_empty() {
        return "''".to_string();
    }
    let is_safe = value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '+' | '%' | '=')
    });
    if is_safe {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Build the final `claude` launch command for `spawn_claude_pane`.
/// Order (matches the issue #137 spec):
///   1. `CLAUDE_PEER_LAUNCH_CMD` (peer-channel flag +
///      `--permission-mode bypassPermissions` baseline, renga-234)
///   2. `--permission-mode <permission_mode>` if present
///   3. `--model <model>` if present
///   4. caller-supplied `args[]`
///
/// The structured `permission_mode` field is emitted after the
/// baseline `--permission-mode bypassPermissions` in the prefix; the
/// Claude CLI resolves duplicate flags later-wins, so a caller-supplied
/// `permission_mode` overrides the default without renga having to
/// strip the baseline. The command line looks slightly noisier when
/// both are present but the semantics stay correct.
///
/// Each value (structured field or extra arg) flows through
/// `shell_quote` so whitespace and shell metacharacters can't
/// re-split the command when the PTY's shell parses it. The
/// `CLAUDE_PEER_LAUNCH_CMD` prefix is a trusted, space-delimited
/// constant and is emitted verbatim.
fn build_claude_launch_command(
    permission_mode: Option<&str>,
    model: Option<&str>,
    extra_args: &[String],
) -> String {
    let mut parts: Vec<String> = vec![CLAUDE_PEER_LAUNCH_CMD.to_string()];
    if let Some(mode) = permission_mode {
        parts.push("--permission-mode".to_string());
        parts.push(shell_quote(mode));
    }
    if let Some(m) = model {
        parts.push("--model".to_string());
        parts.push(shell_quote(m));
    }
    for a in extra_args {
        parts.push(shell_quote(a));
    }
    parts.join(" ")
}

fn build_codex_launch_command(extra_args: &[String]) -> String {
    let mut parts: Vec<String> = vec!["codex".to_string()];
    for a in extra_args {
        parts.push(shell_quote(a));
    }
    parts.join(" ")
}

fn parse_string_args_array(args: &Value) -> std::result::Result<Vec<String>, String> {
    match args.get("args") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (idx, v) in items.iter().enumerate() {
                match v.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(format!("args[{idx}] must be a string, got {v}")),
                }
            }
            Ok(out)
        }
        Some(other) => Err(format!("`args` must be an array of strings; got {other}")),
    }
}

/// TTL for the `claude --help` allowlist cache. Issue #229 calls for
/// "process lifetime or ~5 minutes, whichever is shorter" — we keep
/// the upper bound at 5 minutes so an in-place Claude upgrade that
/// adds new flags is picked up without restarting renga.
const CLAUDE_HELP_TTL: Duration = Duration::from_secs(300);

/// Cache for the parsed allowlist. The Mutex is only held briefly to
/// read or write the cache slot; the (potentially slow) `claude
/// --help` subprocess runs *outside* the lock so concurrent spawns
/// don't serialize on it. A racing double-fetch on cache miss is
/// harmless — the second writer just overwrites the first with the
/// same result.
static CLAUDE_HELP_CACHE: Mutex<Option<(Instant, Arc<HashSet<String>>)>> = Mutex::new(None);

/// Spawn-time soft validation (issue #229): consult `claude --help`
/// and return the set of recognized CLI flags (long forms like
/// `--resume`, short forms like `-p`). Returns `None` when the help
/// text can't be obtained or parsed — the caller falls open in that
/// case rather than blocking the spawn (a missing or upgraded Claude
/// binary should never wedge renga's launch path).
fn claude_help_flag_allowlist() -> Option<Arc<HashSet<String>>> {
    // Fast path: cache hit, lock held briefly.
    {
        let guard = CLAUDE_HELP_CACHE.lock().ok()?;
        if let Some((stamp, set)) = guard.as_ref() {
            if stamp.elapsed() < CLAUDE_HELP_TTL {
                return Some(Arc::clone(set));
            }
        }
    }
    // Slow path: fetch fresh outside the lock so other panes that hit
    // the cache simultaneously aren't blocked behind our subprocess.
    let parsed = match fetch_claude_help_text() {
        Ok(text) => Arc::new(parse_claude_help_flags(&text)),
        Err(e) => {
            log_stderr(&format!(
                "spawn_claude_pane: `claude --help` parse failed; \
                 falling open on flag allowlist ({e})"
            ));
            return None;
        }
    };
    if let Ok(mut guard) = CLAUDE_HELP_CACHE.lock() {
        *guard = Some((Instant::now(), Arc::clone(&parsed)));
    }
    Some(parsed)
}

/// Run `claude --help` and capture stdout. Errors out on missing
/// binary, non-zero exit, or non-UTF-8 output — all of which trigger
/// the fall-open path in `claude_help_flag_allowlist`.
fn fetch_claude_help_text() -> std::result::Result<String, String> {
    let output = Command::new("claude")
        .arg("--help")
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("non-zero exit: {}", output.status));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("non-UTF-8 stdout: {e}"))
}

/// Extract recognized flag tokens from `claude --help` output.
///
/// Every option line in Claude's help starts with whitespace + a flag
/// (e.g. `  --resume   …`, `  -p, --print   …`). We pick those lines
/// up by trimming leading whitespace and checking for a leading `-`,
/// then split on whitespace and commas to walk the flag tokens. The
/// first non-flag token (a value placeholder like `<dir>`, `[name]`,
/// or the start of the description column) marks the boundary.
///
/// The `--foo=value` form is collapsed to its head (`--foo`) so the
/// validator's `head` lookup matches regardless of which form a
/// caller used.
///
/// Subcommand lines (`agents [options]`, `doctor`, …) are skipped
/// because they don't start with `-`. Wrapped description text that
/// happens to mention a `--flag` token is *not* picked up — the help
/// emits each option on a single line, and continuation lines (if
/// any) start with description text rather than a leading dash.
fn parse_claude_help_flags(help_text: &str) -> HashSet<String> {
    let mut flags = HashSet::new();
    for line in help_text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('-') {
            continue;
        }
        // Normalize the comma between aliases (`-p, --print`) so a
        // single split_whitespace pass walks both names.
        let normalized = trimmed.replace(',', " ");
        for tok in normalized.split_whitespace() {
            if !tok.starts_with('-') {
                // Hit the value placeholder or description — done.
                break;
            }
            // Strip the `=value` half so `--foo=bar` registers as
            // `--foo`. Tokens without `=` keep their full form.
            let head = tok.split('=').next().unwrap_or(tok);
            // Bare `-` / `--` aren't real flags; skip them so the
            // allowlist doesn't accidentally accept them.
            if head == "-" || head == "--" {
                continue;
            }
            flags.insert(head.to_string());
        }
    }
    flags
}

/// Render an abbreviated, sorted view of an allowlist for use in the
/// `[invalid-params]` error message. Keeps the response small — full
/// dumps of ~50 Claude flags would crowd the agent's context.
fn abbreviate_flag_list(allowed: &HashSet<String>) -> String {
    const MAX: usize = 12;
    let mut sorted: Vec<&str> = allowed.iter().map(String::as_str).collect();
    sorted.sort();
    if sorted.len() > MAX {
        let head_list = sorted[..MAX].join(", ");
        format!("{head_list}, … ({} more)", sorted.len() - MAX)
    } else {
        sorted.join(", ")
    }
}

/// Parse the `args` JSON array for `spawn_claude_pane`, rejecting:
///
/// 1. Entries that match a structured-field flag (`--permission-mode`,
///    `--model`, `--dangerously-load-development-channels`) — these
///    are owned by the structured fields, and letting `args[]` also
///    inject them produces ambiguous command lines.
/// 2. Flag-shaped entries (`-x` / `--foo` / `--foo=bar`) that don't
///    appear in the soft-validation allowlist (`claude --help` output)
///    — protects callers from typos and silently-forwarded unknown
///    flags that surface as a Claude exit-1 inside the spawned pane
///    (issue #229).
///
/// Both checks match on the head (the chunk before any `=`) so a
/// caller can't sneak a reserved or unknown flag through by combining
/// it with its value. Non-flag args (positional values, prompts,
/// paths) pass through unconditionally.
///
/// `allowlist == None` disables soft validation — used both by the
/// fall-open path when `claude --help` fails and by tests that want
/// to exercise the reserved-flag branch in isolation.
fn validate_claude_extra_args(
    args: &[String],
    allowlist: Option<&HashSet<String>>,
) -> std::result::Result<(), String> {
    for a in args {
        // `split('=')` always yields at least one element, so
        // `next().unwrap_or("")` degrades to an empty head for inputs
        // that start with `=` or are empty — neither of which matches
        // any reserved flag or starts with `-`, so both checks below
        // fall through cleanly to "allowed".
        let head = a.split('=').next().unwrap_or("");
        if CLAUDE_RESERVED_FLAGS.contains(&head) {
            return Err(format!(
                "args[] must not contain {head:?} — pass it via the structured field \
                 ({}) instead",
                match head {
                    "--permission-mode" => "permission_mode",
                    "--model" => "model",
                    "--dangerously-load-development-channels" =>
                        "implicit (always added by spawn_claude_pane)",
                    _ => "<structured>",
                }
            ));
        }
        // Soft validation: only kicks in when the token looks like a
        // flag *and* an allowlist is available. Positional values
        // (prompts, paths) and the fall-open path both pass through.
        if let Some(allowed) = allowlist {
            if a.starts_with('-') && !allowed.contains(head) {
                return Err(format!(
                    "unknown Claude CLI flag {head:?}; valid options from `claude --help`: {}",
                    abbreviate_flag_list(allowed)
                ));
            }
        }
    }
    Ok(())
}

fn handle_spawn_claude_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let direction = match parse_direction(args.get("direction").and_then(|v| v.as_str())) {
        Ok(d) => d,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let target = parse_target(args.get("target").and_then(|v| v.as_str()));
    let name = opt_string(args, "name");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");
    let permission_mode = opt_string(args, "permission_mode");
    let model = opt_string(args, "model");

    // `args` must be a JSON array of strings when present — reject
    // anything else instead of silently coercing, so typos surface.
    let extra_args = match parse_string_args_array(args) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    // Skip the `claude --help` round-trip when there are no caller-
    // supplied args — there's nothing to validate against, and we'd
    // rather not pay the spawn-time cost on the trivial path.
    let allowlist = if extra_args.is_empty() {
        None
    } else {
        claude_help_flag_allowlist()
    };
    if let Err(msg) = validate_claude_extra_args(&extra_args, allowlist.as_deref()) {
        return err_response(id, -32602, &msg);
    }

    let command =
        build_claude_launch_command(permission_mode.as_deref(), model.as_deref(), &extra_args);

    let (caller_pane, endpoint) = match require_connected(ctx, id, "spawn claude pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // Relative cwd resolution mirrors `spawn_pane` so the two tools
    // give identical path semantics; only the command construction
    // differs.
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    match client::send_request(
        endpoint,
        &Request::Split {
            target,
            direction,
            command: Some(command.clone()),
            id: name,
            role,
            cwd,
        },
    ) {
        Ok(Response::Ok { data }) => spawn_claude_pane_ok_response(id, &data, &command),
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!(
                "renga refused spawn_claude_pane: {}",
                fmt_code(&message, &code)
            ),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn spawn_claude_pane_ok_response(id: &Value, data: &Value, command: &str) -> Value {
    let new_id = data.get("id").and_then(|v| v.as_u64());
    let pane = created_pane_text(new_id);
    let msg = spawn_startup_message(&pane, Some("Claude"), data, Some(command));
    ok_response(id, tool_text_result(&msg))
}

fn handle_spawn_codex_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    handle_spawn_codex_pane_with(id, args, ctx, install::verify_codex_renga_peers_install)
}

fn spawn_codex_pane_ok_response(id: &Value, data: &Value, command: &str) -> Value {
    let new_id = data.get("id").and_then(|v| v.as_u64());
    let pane = created_pane_text(new_id);
    let msg = spawn_startup_message(&pane, Some("Codex"), data, Some(command));
    ok_response(id, tool_text_result(&msg))
}

/// Inner form with an injectable verifier so unit tests can drive the
/// `RENGA_PEER_CLIENT_KIND` check independently of the host machine's
/// `~/.codex/config.toml`.
fn handle_spawn_codex_pane_with(
    id: &Value,
    args: &Value,
    ctx: &PeerCtx,
    verify_codex_install: fn() -> std::result::Result<(), String>,
) -> Value {
    let direction = match parse_direction(args.get("direction").and_then(|v| v.as_str())) {
        Ok(d) => d,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let target = parse_target(args.get("target").and_then(|v| v.as_str()));
    let name = opt_string(args, "name");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");
    let extra_args = match parse_string_args_array(args) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let command = build_codex_launch_command(&extra_args);

    let (caller_pane, endpoint) = match require_connected(ctx, id, "spawn codex pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // Issue #203: refuse to spawn unless Codex's MCP config will
    // inject `RENGA_PEER_CLIENT_KIND=codex` into the new pane's
    // mcp-peer subprocess. Otherwise the new pane registers as a
    // push (claude) client and `send_message` delivery silently
    // bifurcates from what the orchestrator expects. Runs after the
    // detached/connected gate so a renga-not-reachable failure isn't
    // hidden by a spurious `[codex_not_installed]`.
    if let Err(reason) = verify_codex_install() {
        // Always surface the remediation command — the verifier's
        // detail string explains *which* check failed (file missing /
        // entry missing / wrong value), but the user-actionable
        // recovery is always the same.
        return err_response(
            id,
            -32603,
            &format!(
                "renga refused spawn_codex_pane: [codex_not_installed] {reason} \
                 (run `renga mcp install --client codex` to register Codex \
                 with `RENGA_PEER_CLIENT_KIND=codex`)"
            ),
        );
    }
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    match client::send_request(
        endpoint,
        &Request::Split {
            target,
            direction,
            command: Some(command.clone()),
            id: name,
            role,
            cwd,
        },
    ) {
        Ok(Response::Ok { data }) => spawn_codex_pane_ok_response(id, &data, &command),
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!(
                "renga refused spawn_codex_pane: {}",
                fmt_code(&message, &code)
            ),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn handle_close_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    if raw.trim().is_empty() {
        return err_response(
            id,
            -32602,
            "close_pane requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(raw));
    let (_caller_pane, endpoint) = match require_connected(ctx, id, "close pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request(endpoint, &Request::Close { target }) {
        Ok(Response::Ok { data }) => {
            let closed_id = data.get("id").and_then(|v| v.as_u64());
            let msg = match closed_id {
                Some(n) => format!("Closed pane id={n}."),
                None => "Closed pane.".to_string(),
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused close_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn handle_focus_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return err_response(
            id,
            -32602,
            "focus_pane requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(trimmed));
    let (_caller_pane, endpoint) = match require_connected(ctx, id, "focus pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request(endpoint, &Request::Focus { target }) {
        // Focus replies with `ok_unit` per the IPC contract (see
        // `src/ipc/server.rs`), so there's no resolved id to echo.
        // Echoing the trimmed user input is the most informative thing
        // we can do without a second round-trip.
        Ok(Response::Ok { .. }) => {
            ok_response(id, tool_text_result(&format!("Focused {trimmed}.")))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused focus_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn handle_new_tab(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    handle_new_tab_with_request(id, args, ctx, client::send_request)
}

fn handle_new_tab_with_request<F>(id: &Value, args: &Value, ctx: &PeerCtx, send_request: F) -> Value
where
    F: FnOnce(&EndpointName, &Request) -> Result<Response>,
{
    let command = opt_string(args, "command").map(|c| upgrade_claude_command(&c));
    let name = opt_string(args, "name");
    let label = opt_string(args, "label");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");
    let no_startup_command_possible = command.is_none() && role.is_none();

    let (caller_pane, endpoint) = match require_connected(ctx, id, "open new tab") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    match send_request(
        endpoint,
        &Request::NewTab {
            command: command.clone(),
            id: name,
            label,
            role,
            cwd,
        },
    ) {
        Ok(Response::Ok { data }) => {
            new_tab_ok_response(id, &data, command.as_deref(), no_startup_command_possible)
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused new_tab: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

fn new_tab_ok_response(
    id: &Value,
    data: &Value,
    requested_command: Option<&str>,
    no_startup_command_possible: bool,
) -> Value {
    let new_id = data.get("id").and_then(|v| v.as_u64());
    let tab = match new_id {
        Some(n) => format!("Opened new tab; new pane id={n} (now focused)."),
        None => "Opened new tab.".to_string(),
    };
    let msg = if no_startup_command_possible
        && !matches!(data.get("startup_command"), Some(Value::String(_)))
    {
        format!("{tab} No startup command requested.")
    } else {
        spawn_startup_message(&tab, None, data, requested_command)
    };
    ok_response(id, tool_text_result(&msg))
}

// ── set_pane_identity (rename / re-assign role) ──────────────

/// Three-state arg extractor for `set_pane_identity`. Maps:
///
/// - key absent → `None`                       (leave unchanged)
/// - key present & JSON null → `Some(None)`    (clear)
/// - key present & JSON string → `Some(Some))` (set)
///
/// Any other JSON type (number, bool, object, array) is rejected —
/// the schema forbids it but Claude might still try, and silently
/// accepting a coerced value would confuse the three-state contract.
fn parse_identity_field(
    args: &Value,
    key: &str,
) -> std::result::Result<Option<Option<String>>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(other) => Err(format!("`{key}` must be a string or null; got {}", other)),
    }
}

fn handle_set_pane_identity(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let target = parse_target(args.get("target").and_then(|v| v.as_str()));
    let name = match parse_identity_field(args, "name") {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let role = match parse_identity_field(args, "role") {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    if name.is_none() && role.is_none() {
        // Nothing to do — return an explicit error so Claude doesn't
        // silently succeed on a typo'd payload (`nmae` instead of
        // `name`, etc.). The server would otherwise treat it as a
        // valid "no-op" call.
        return err_response(
            id,
            -32602,
            "set_pane_identity requires at least one of `name` / `role`",
        );
    }

    let (_caller_pane, endpoint) = match require_connected(ctx, id, "set pane identity") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request(endpoint, &Request::SetPaneIdentity { target, name, role }) {
        Ok(Response::Ok { data }) => {
            // Surface the updated pane record as a human-readable
            // line so Claude can confirm the new identity without
            // parsing structuredContent.
            let pane = data.get("pane").cloned().unwrap_or(Value::Null);
            let pane_id = pane.get("id").and_then(|v| v.as_u64());
            let pane_name = pane
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let pane_role = pane
                .get("role")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mut parts = Vec::new();
            if let Some(n) = pane_id {
                parts.push(format!("id={n}"));
            }
            parts.push(format!("name={}", pane_name.as_deref().unwrap_or("(none)")));
            parts.push(format!("role={}", pane_role.as_deref().unwrap_or("(none)")));
            let msg = format!("Updated pane: {}.", parts.join(" "));
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!(
                "renga refused set_pane_identity: {}",
                fmt_code(&message, &code)
            ),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

// ── set_summary (per-pane summary string) ────────────────────

fn handle_set_summary(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    // The schema requires `summary` as a string. Reject anything else
    // (number, null, etc.) explicitly so callers get a clear error
    // rather than silently coercing.
    let summary = match args.get("summary") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return err_response(
                id,
                -32602,
                &format!("`summary` must be a string; got {}", other),
            );
        }
        None => {
            return err_response(id, -32602, "set_summary requires a `summary` argument");
        }
    };

    let (caller_pane, endpoint) = match require_connected(ctx, id, "set summary") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request(
        endpoint,
        &Request::SetSummary {
            from_pane: caller_pane,
            summary: summary.clone(),
        },
    ) {
        Ok(Response::Ok { .. }) => {
            let msg = if summary.is_empty() {
                "Summary cleared.".to_string()
            } else {
                format!("Summary set: {summary}")
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused set_summary: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

// ── inspect_pane (pane screen snapshot over MCP) ──────────────

/// Cap on the `lines` argument. The underlying screen is bounded by
/// the pane's terminal height (< 1000 under any sane desktop), but
/// accept a generous ceiling so callers can request "everything I can
/// possibly see" without hand-tuning. Values above this are clamped
/// silently to match how `renga inspect --lines` treats oversized
/// requests.
const INSPECT_MAX_LINES: u64 = 10_000;

fn parse_inspect_format(raw: Option<&str>) -> std::result::Result<InspectFormat, String> {
    match raw.map(str::trim) {
        None | Some("") | Some("text") => Ok(InspectFormat::Text),
        Some("grid") => Ok(InspectFormat::Grid),
        Some(other) => Err(format!(
            "invalid format {other:?}; expected 'text' or 'grid'"
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectFormat {
    Text,
    Grid,
}

/// Render the Inspect IPC payload's `text` field as the content
/// block, defaulting to an empty string when absent so Claude
/// never sees a missing field crash.
fn inspect_text_block(payload: &Value) -> String {
    payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Render the Inspect IPC payload's `lines` array as a
/// human-inspectable JSON grid. Falls back to the raw payload text
/// when the array is absent so a malformed payload doesn't silently
/// produce an empty response.
fn inspect_grid_block(payload: &Value) -> String {
    match payload.get("lines") {
        Some(lines) => serde_json::to_string_pretty(lines).unwrap_or_else(|_| lines.to_string()),
        None => inspect_text_block(payload),
    }
}

fn handle_inspect_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw_target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    if raw_target.trim().is_empty() {
        return err_response(
            id,
            -32602,
            "inspect_pane requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(raw_target));
    let lines = args.get("lines").and_then(|v| v.as_u64()).map(|n| {
        let clamped = n.min(INSPECT_MAX_LINES);
        clamped as usize
    });
    let include_cursor = args
        .get("include_cursor")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let format = match parse_inspect_format(args.get("format").and_then(|v| v.as_str())) {
        Ok(f) => f,
        Err(msg) => return err_response(id, -32602, &msg),
    };

    let (_caller_pane, endpoint) = match require_connected(ctx, id, "inspect pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    match client::send_request(
        endpoint,
        &Request::Inspect {
            target,
            lines,
            include_cursor,
        },
    ) {
        Ok(Response::Ok { data }) => {
            let text = match format {
                InspectFormat::Text => inspect_text_block(&data),
                InspectFormat::Grid => inspect_grid_block(&data),
            };
            ok_response(
                id,
                json!({
                    "content": [ { "type": "text", "text": text } ],
                    "isError": false,
                    "structuredContent": data,
                }),
            )
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused inspect_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

// ── send_keys (raw PTY key input over MCP) ────────────────────

/// Translate a named special-key token into the byte sequence that a
/// VT-style terminal expects. Returns `None` for unknown names so the
/// caller surfaces a -32602 invalid-params error with the verbatim
/// input.
///
/// The vocabulary is intentionally conservative — the named set
/// covers the keys aainc-ops-style orchestrators actually need today
/// (y/n answers, Shift+Tab for Claude Code's Plan → AcceptEdits
/// toggle, Esc, arrow keys for menus, Ctrl+<letter> for signalling).
/// Escape sequences match xterm's default mode (no application-cursor
/// quirks) since that is what renga's vt100 parser speaks.
fn translate_key(name: &str) -> Option<String> {
    let trimmed = name.trim();
    match trimmed {
        // Raw-mode TUIs read bytes directly from the PTY — including
        // Claude Code, which is the prime target here — so Enter must
        // be carriage return (CR, 0x0D), not line feed. This matches
        // what renga's own `Request::Send { append_enter: true }`
        // writes on the send path.
        "Enter" | "Return" => return Some("\r".into()),
        "Tab" => return Some("\t".into()),
        "Shift+Tab" | "BackTab" => return Some("\x1b[Z".into()),
        "Esc" | "Escape" => return Some("\x1b".into()),
        "Backspace" => return Some("\x7f".into()),
        "Delete" | "Del" => return Some("\x1b[3~".into()),
        "Up" => return Some("\x1b[A".into()),
        "Down" => return Some("\x1b[B".into()),
        "Right" => return Some("\x1b[C".into()),
        "Left" => return Some("\x1b[D".into()),
        "Home" => return Some("\x1b[H".into()),
        "End" => return Some("\x1b[F".into()),
        "PageUp" => return Some("\x1b[5~".into()),
        "PageDown" => return Some("\x1b[6~".into()),
        "Space" => return Some(" ".into()),
        _ => {}
    }
    if let Some(suffix) = trimmed.strip_prefix("Ctrl+") {
        let mut chars = suffix.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_alphabetic() {
                let byte = (upper as u8) - b'A' + 1;
                return Some(String::from(byte as char));
            }
        }
    }
    None
}

/// Assemble the final byte stream to push at the target pane from the
/// tool arguments. Returns an error string on an unknown key or an
/// empty request (no text, no keys, no enter) so the caller produces a
/// -32602 JSON-RPC error without an IPC round-trip.
pub(crate) fn build_send_keys_payload(
    text: &str,
    keys: Option<&[Value]>,
    append_enter: bool,
) -> std::result::Result<String, String> {
    let mut buffer = String::from(text);
    if let Some(keys) = keys {
        for key in keys {
            let name = key
                .as_str()
                .ok_or_else(|| format!("send_keys.keys elements must be strings; got {key:?}"))?;
            let bytes = translate_key(name).ok_or_else(|| {
                format!(
                    "send_keys: unknown key {name:?}. See the tool description for the supported vocabulary."
                )
            })?;
            buffer.push_str(&bytes);
        }
    }
    if append_enter {
        // Mirror the Enter key mapping above: raw-mode TUIs want CR,
        // not LF. Using \r here also keeps this path byte-identical
        // to `Request::Send { append_enter: true }` in renga itself,
        // so callers don't have to reason about two Enter dialects.
        buffer.push('\r');
    }
    if buffer.is_empty() {
        return Err(
            "send_keys requires at least one of `text`, a non-empty `keys` array, or `enter=true`"
                .into(),
        );
    }
    Ok(buffer)
}

fn handle_send_keys(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw_target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    if raw_target.trim().is_empty() {
        return err_response(
            id,
            -32602,
            "send_keys requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(raw_target));

    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let keys = args.get("keys").and_then(|v| v.as_array());
    let enter = args.get("enter").and_then(|v| v.as_bool()).unwrap_or(false);

    let payload = match build_send_keys_payload(text, keys.map(|v| v.as_slice()), enter) {
        Ok(p) => p,
        Err(msg) => return err_response(id, -32602, &msg),
    };

    let (_caller_pane, endpoint) = match require_connected(ctx, id, "send keys") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request(
        endpoint,
        &Request::Send {
            target,
            data: payload,
            // We assemble the Enter bit into `payload` above so every
            // call path (text-only / keys-only / combined) takes the
            // same branch server-side. `append_enter` stays false.
            append_enter: false,
        },
    ) {
        Ok(Response::Ok { .. }) => ok_response(
            id,
            tool_text_result(&format!("Sent keys to {}.", raw_target.trim())),
        ),
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused send_keys: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

// ── poll_events (long-poll over buffered lifecycle events) ────

/// Outcome of a single buffer scan. Separated from the tool response
/// so the scan can be written as a pure function against a locked
/// `EventBuffer`, independent of the long-poll / timeout / JSON shape.
#[derive(Debug, PartialEq)]
struct PollScan {
    /// Events in the window (seq >= start_cursor) that matched the
    /// optional `types` filter.
    matched: Vec<Value>,
    /// Highest seq in the window regardless of filter. `None` when no
    /// events fall in the window at all. When `Some`, this becomes the
    /// response's `next_since` so filtered-out events don't make the
    /// caller re-scan the same range.
    window_max_seq: Option<u64>,
}

fn scan_buffer(buf: &EventBuffer, start_cursor: u64, types_filter: Option<&[String]>) -> PollScan {
    let mut matched = Vec::new();
    let mut window_max_seq: Option<u64> = None;
    for e in &buf.events {
        if e.seq < start_cursor {
            continue;
        }
        window_max_seq = Some(window_max_seq.map_or(e.seq, |prev| prev.max(e.seq)));
        if event_matches_filter(&e.value, types_filter) {
            matched.push(e.value.clone());
        }
    }
    PollScan {
        matched,
        window_max_seq,
    }
}

fn event_matches_filter(event: &Value, filter: Option<&[String]>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    if filter.is_empty() {
        return true;
    }
    let Some(ty) = event.get("type").and_then(|v| v.as_str()) else {
        return false;
    };
    filter.iter().any(|f| f == ty)
}

fn poll_events_payload(events: Vec<Value>, next_since: u64) -> Value {
    let body = json!({
        "next_since": next_since.to_string(),
        "events": events,
    });
    let text = serde_json::to_string(&body).unwrap_or_else(|_| body.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": false,
        "structuredContent": body,
    })
}

/// Compute the effective long-poll duration from a caller-supplied
/// `timeout_ms`. Missing → default; oversize → clamped to the hard
/// cap. Factored out so the clamping can be unit-tested without
/// actually blocking a test thread for the full cap.
fn effective_poll_timeout(requested: Option<u64>) -> Duration {
    let ms = requested
        .unwrap_or(POLL_DEFAULT_TIMEOUT_MS)
        .min(POLL_MAX_TIMEOUT_MS);
    Duration::from_millis(ms)
}

fn handle_poll_events(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let since = args
        .get("since")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let timeout = effective_poll_timeout(args.get("timeout_ms").and_then(|v| v.as_u64()));
    let types_filter: Option<Vec<String>> =
        args.get("types").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });

    // Detached mode: no subscriber thread is running, so the buffer
    // will stay empty forever. Return immediately with a cursor of 0
    // rather than blocking the stdio dispatcher for `timeout_ms`.
    if matches!(ctx.mode, Mode::Detached { .. }) {
        return ok_response(id, poll_events_payload(Vec::new(), since.unwrap_or(0)));
    }

    let (lock, cvar) = &*ctx.events;
    let mut buf = lock.lock().unwrap_or_else(|p| p.into_inner());

    // Start inclusive lower bound. `since` is "the highest seq the
    // caller already knows about", so the next delivery window is
    // `since + 1`. `since = None` means "no history — give me events
    // that arrive after this call".
    let start_cursor = match since {
        Some(s) => s.saturating_add(1),
        None => buf.last_seq.saturating_add(1),
    };

    let deadline = Instant::now() + timeout;
    loop {
        let scan = scan_buffer(&buf, start_cursor, types_filter.as_deref());
        if let Some(max_seq) = scan.window_max_seq {
            return ok_response(id, poll_events_payload(scan.matched, max_seq));
        }

        let now = Instant::now();
        if now >= deadline {
            // Timeout with no events in window. Hold the cursor where
            // it was so the next call resumes from the same point.
            let next = start_cursor.saturating_sub(1);
            return ok_response(id, poll_events_payload(Vec::new(), next));
        }
        let remaining = deadline - now;
        buf = match cvar.wait_timeout(buf, remaining) {
            Ok((g, _)) => g,
            Err(p) => p.into_inner().0,
        };
    }
}

// ── stdin dispatch loop ───────────────────────────────────────

fn dispatch(req: &Value, ctx: &PeerCtx) -> Result<Vec<Value>> {
    let is_notification = req.get("id").is_none();
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            if is_notification {
                log_stderr("dropping malformed notification with no method");
                return Ok(Vec::new());
            }
            return Ok(vec![err_response(
                &id,
                -32600,
                "invalid request: missing or non-string 'method'",
            )]);
        }
    };
    let params = req.get("params").cloned().unwrap_or(json!({}));
    if is_notification {
        // Lifecycle notifications are accepted silently; unknown ones logged.
        if matches!(method, "notifications/initialized" | "initialized") {
            let subscribed = mark_push_initialized(ctx);
            if ctx.client_kind.receive_mode() == ipc::PeerReceiveMode::Push && subscribed {
                schedule_deferred_push_ready(ctx);
            }
        } else if !matches!(method, "notifications/cancelled" | "$/cancel") {
            log_stderr(&format!("ignored unknown notification: {method}"));
        }
        return Ok(Vec::new());
    }
    let frames = match method {
        "initialize" => vec![handle_initialize(&id, &params, ctx)],
        "tools/list" => vec![handle_tools_list(&id)],
        "tools/call" => vec![handle_tools_call(&id, &params, ctx)?],
        "ping" => vec![ok_response(&id, json!({}))],
        other => vec![err_response(
            &id,
            -32601,
            &format!("method not found: {other}"),
        )],
    };
    Ok(frames)
}

fn stdio_loop(ctx: &PeerCtx) -> Result<()> {
    let stdin = io::stdin();
    let reader = stdin.lock();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                log_stderr(&format!("stdin read error: {e}"));
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                log_stderr(&format!("malformed JSON frame: {e} — raw={trimmed}"));
                // JSON-RPC 2.0 §5.1: on parse error, the server MUST
                // respond with id=null + code -32700. Clients that
                // correlate replies by id will otherwise hang.
                let parse_err = err_response(&Value::Null, -32700, &format!("parse error: {e}"));
                let _ = write_frame(&parse_err);
                continue;
            }
        };
        match dispatch(&value, ctx) {
            Ok(frames) => {
                for f in &frames {
                    if let Err(e) = write_frame(f) {
                        log_stderr(&format!("failed to write frame: {e}"));
                    }
                }
            }
            Err(e) => {
                log_stderr(&format!("dispatch error: {e}"));
                if let Some(id) = value.get("id") {
                    let payload = err_response(id, -32603, &format!("internal error: {e}"));
                    let _ = write_frame(&payload);
                }
            }
        }
    }
    log_stderr("stdin closed; exiting");
    Ok(())
}

// ── event bus subscriber (background thread) ──────────────────

fn handle_peer_subscription_event(
    registration_ctx: &PeerCtx,
    ack_sender: &PeerInboxAckSender,
    receipt_cache: &mut PeerReceiptCache,
    inbox: &InboxSink,
    client_kind: PeerClientKind,
    pane_id: usize,
    event: ipc::Event,
) -> Option<ipc::Event> {
    let ipc::Event::PeerInbox {
        delivery_id,
        target_pane,
        from_pane,
        from_name,
        from_kind,
        body,
        ts_ms,
    } = event
    else {
        return Some(event);
    };
    if target_pane != pane_id {
        return None;
    }
    let debug_metadata = registration_ctx.debug_log_path.as_ref().map(|_| {
        (
            delivery_id.is_some_and(|id| receipt_cache.contains(id)),
            body.len(),
        )
    });
    let retained = retain_peer_delivery_once(receipt_cache, delivery_id, || {
        if client_kind.receive_mode() == ipc::PeerReceiveMode::Pull {
            queue_pull_message(
                inbox,
                QueuedPeerMessage {
                    delivery_id,
                    from_id: from_pane.to_string(),
                    from_name: from_name.clone(),
                    from_kind,
                    body: body.clone(),
                    sent_at: ts_ms_to_string(ts_ms),
                },
            );
            true
        } else {
            let note = channel_notification(&body, &from_pane.to_string(), from_name.as_deref());
            deliver_push_frame(registration_ctx, note, delivery_id, "peer_inbox")
        }
    });
    if let Some((receipt_cache_hit, body_len)) = debug_metadata {
        let inbox_len_after = inbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .messages
            .len();
        log_peer_inbox_received(
            registration_ctx,
            delivery_id,
            from_pane,
            body_len,
            inbox_len_after,
        );
        if receipt_cache_hit {
            log_peer_receipt_cache_hit(registration_ctx, delivery_id);
        }
    }
    if retained {
        if let Some(delivery_id) = delivery_id {
            ack_sender.enqueue(delivery_id);
        }
    }
    None
}

/// Subscribe to renga's event bus and push any [`ipc::Event::PeerInbox`]
/// whose `target_pane` matches our own pane id as a
/// `notifications/claude/channel` frame on stdout. The thread is
/// detached — it dies naturally when the IPC stream closes (renga
/// exited) or when the subprocess is killed.
fn spawn_inbox_subscriber(ctx: PeerCtx) {
    let Mode::Connected { pane_id, endpoint } = ctx.mode.clone() else {
        return;
    };
    let endpoint_clone = endpoint.clone();
    let sink = ctx.events.clone();
    let inbox = ctx.inbox.clone();
    let client_kind = ctx.client_kind;
    let registration_ctx = ctx.clone();
    thread::Builder::new()
        .name("renga-mcp-peer-inbox".into())
        .spawn(move || {
            let mut consecutive_failures = 0u32;
            let mut retry_delay = Duration::from_millis(250);
            let mut receipt_cache = PeerReceiptCache::default();
            loop {
                let attempt_started = Instant::now();
                let ack_sender = spawn_peer_inbox_ack_sender(registration_ctx.clone());
                let subscribed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let subscribed_on_ready = subscribed.clone();
                let result = client::subscribe_peer_events_with_ready(
                &endpoint_clone,
                pane_id,
                || {
                    subscribed_on_ready.store(true, std::sync::atomic::Ordering::Release);
                    register_client_kind(&registration_ctx);
                    reconcile_peer_inbox(&registration_ctx);
                    publish_ready_after_subscribe(&registration_ctx);
                },
                |event| {
                // Buffer lifecycle events for `poll_events` before we
                // consume `event` in the match below. Heartbeat is a
                // wire-keepalive (not a lifecycle signal) and PeerInbox
                // is delivered out-of-band via channel notifications,
                // so neither belongs in the poll buffer. Everything
                // else — PaneStarted / PaneExited / EventsDropped plus
                // any forward-compatible variants added later — gets
                // stashed.
                if should_buffer_for_poll(&event) {
                    match serde_json::to_value(&event) {
                        Ok(value) => {
                            let (lock, cvar) = &*sink;
                            let mut buf = lock.lock().unwrap_or_else(|p| p.into_inner());
                            buf.push(value);
                            cvar.notify_all();
                        }
                        Err(e) => log_stderr(&format!(
                            "failed to serialize event for poll buffer: {e}"
                        )),
                    }
                }
                let Some(event) = handle_peer_subscription_event(
                    &registration_ctx,
                    &ack_sender,
                    &mut receipt_cache,
                    &inbox,
                    client_kind,
                    pane_id,
                    event,
                ) else {
                    return true;
                };
                if let ipc::Event::EventsDropped { count, .. } = event {
                    // The EventBus bounds each subscriber at 256 events
                    // and drops new events for slow consumers, reporting
                    // the gap via EventsDropped. If this thread couldn't
                    // keep up, a peer message may have been silently
                    // lost — surface that as a channel notice so Claude
                    // knows to ask the peer to resend instead of
                    // assuming all is well.
                        log_stderr(&format!(
                            "event bus dropped {count} event(s) due to slow subscriber"
                        ));
                        let body = format!(
                            "renga event bus dropped {count} event(s) before they reached this peer client. A peer message may have been lost — consider asking the sender to retry."
                        );
                        if client_kind.receive_mode() == ipc::PeerReceiveMode::Pull {
                            queue_pull_message(&inbox, QueuedPeerMessage {
                                delivery_id: None,
                                from_id: "renga".to_string(),
                                from_name: Some("renga runtime".to_string()),
                                from_kind: None,
                                body,
                                sent_at: now_ts_string(),
                            });
                        } else {
                            let note = channel_notification(&body, "renga", Some("renga runtime"));
                            deliver_push_frame(
                                &registration_ctx,
                                note,
                                None,
                                "events_dropped",
                            );
                        }
                }
                    true
                },
            );
                let was_subscribed = subscribed.load(std::sync::atomic::Ordering::Acquire);
                let attempt_elapsed = attempt_started.elapsed();
                if was_subscribed {
                    if registration_ctx.client_kind.receive_mode() == ipc::PeerReceiveMode::Push {
                        revoke_push_ready(&registration_ctx);
                    } else {
                        set_client_ready(&registration_ctx, false);
                    }
                }
                ack_sender.finish();
                (consecutive_failures, retry_delay) = subscription_retry_state_after_attempt(
                    consecutive_failures,
                    retry_delay,
                    was_subscribed,
                    attempt_elapsed,
                );
                if was_subscribed {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures.is_power_of_two() {
                        log_stderr(&format!(
                            "event stream closed after subscribing; retry {consecutive_failures} in {retry_delay:?}"
                        ));
                    }
                } else {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures.is_power_of_two() {
                        match &result {
                            Ok(()) => log_stderr(&format!(
                                "event subscription closed before ready; retry {consecutive_failures} in {retry_delay:?}"
                            )),
                            Err(e) => log_stderr(&format!(
                                "event subscription unavailable: {e}; retry {consecutive_failures} in {retry_delay:?}"
                            )),
                        }
                    }
                }
                thread::sleep(retry_delay);
                retry_delay = next_subscription_retry_delay(retry_delay);
            }
        })
        .expect("spawn inbox subscriber thread");
}

fn log_peer_inbox_received(
    ctx: &PeerCtx,
    delivery_id: Option<u64>,
    from_pane: usize,
    body_len: usize,
    inbox_len_after: usize,
) {
    let Some(path) = ctx.debug_log_path.as_deref() else {
        return;
    };
    append_peer_debug_record(
        path,
        peer_ctx_pane_id(ctx),
        json!({
            "action": "peer_inbox_received",
            "delivery_id": delivery_id,
            "from_pane": from_pane,
            "body_len": body_len,
            "inbox_len_after": inbox_len_after,
        }),
    );
}

fn log_peer_receipt_cache_hit(ctx: &PeerCtx, delivery_id: Option<u64>) {
    let Some(path) = ctx.debug_log_path.as_deref() else {
        return;
    };
    append_peer_debug_record(
        path,
        peer_ctx_pane_id(ctx),
        json!({
            "action": "peer_receipt_cache_hit",
            "delivery_id": delivery_id,
        }),
    );
}

/// True for events that belong in the `poll_events` ring buffer. A
/// free function so tests can pin the classification without spinning
/// up a subscriber thread.
fn should_buffer_for_poll(event: &ipc::Event) -> bool {
    !matches!(
        event,
        ipc::Event::Heartbeat { .. } | ipc::Event::PeerInbox { .. }
    )
}

fn next_subscription_retry_delay(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(30))
}

fn subscription_retry_state_after_attempt(
    consecutive_failures: u32,
    retry_delay: Duration,
    was_subscribed: bool,
    lifetime: Duration,
) -> (u32, Duration) {
    if was_subscribed && lifetime >= Duration::from_secs(30) {
        (0, Duration::from_millis(250))
    } else {
        (consecutive_failures, retry_delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    struct EnvVarRestore {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarRestore {
        fn set(name: &'static str, value: &std::ffi::OsStr) -> Self {
            let previous = std::env::var_os(name);
            unsafe { std::env::set_var(name, value) };
            Self { name, previous }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.name, value) },
                None => unsafe { std::env::remove_var(self.name) },
            }
        }
    }

    #[test]
    fn peer_send_result_only_claims_delivered_for_explicit_outcome() {
        assert_eq!(
            peer_send_result_text("2", &json!({ "delivery": "delivered" })),
            "Delivered to 2."
        );
        assert_eq!(
            peer_send_result_text("2", &json!({ "delivery": "queued" })),
            "Queued for 2 (peer client not registered yet)."
        );
        assert_eq!(
            peer_send_result_text("2", &json!({ "delivery": "pending_user_confirmation" })),
            "Pending user confirmation for 2."
        );
        assert_eq!(
            peer_send_result_text("2", &json!({ "delivery": "undeliverable" })),
            "Not delivered to 2: no such pane in this tab (pane ids and names are tab-scoped)."
        );

        for data in [json!({}), json!({ "delivery": "future_value" })] {
            let text = peer_send_result_text("2", &data);
            assert!(text.contains("delivery state unconfirmed"), "{text}");
            assert!(!text.contains("Delivered"), "{text}");
        }
    }

    #[test]
    fn peer_event_timestamp_preserves_original_milliseconds() {
        assert_eq!(ts_ms_to_string(1_725_000_123_456), "1725000123.456000000");
    }

    #[test]
    fn push_delivery_buffers_until_initialized_notification() {
        let ctx = connected_ctx_with(new_event_sink());
        let notification = channel_notification("queued", "2", Some("worker"));

        deliver_push_frame(&ctx, notification.clone(), Some(7), "peer_inbox");

        let state = ctx.push.lock().unwrap();
        assert!(!state.initialized);
        assert_eq!(
            state.pending.front().map(|frame| &frame.value),
            Some(&notification)
        );
    }

    #[test]
    fn initialized_flush_preserves_buffered_fifo_order() {
        let ctx = connected_ctx_with(new_event_sink());
        let first = channel_notification("first", "2", None);
        let second = channel_notification("second", "2", None);
        deliver_push_frame(&ctx, first.clone(), Some(1), "peer_inbox");
        deliver_push_frame(&ctx, second.clone(), Some(2), "peer_inbox");

        let mut emitted = Vec::new();
        let ready = mark_push_initialized_with(&ctx, |value| {
            emitted.push(value.clone());
            Ok(())
        });

        assert!(!ready, "initialization alone is not delivery readiness");
        assert_eq!(emitted, vec![first, second]);
        let state = ctx.push.lock().unwrap();
        assert!(state.initialized);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn push_debug_log_records_buffer_flush_emit_and_lifecycle_order() {
        let path = debug_test_path("push-lifecycle");
        let ctx = connected_ctx_with_debug_log(path.clone());
        let notification = channel_notification("first", "2", None);

        assert!(deliver_push_frame(
            &ctx,
            notification,
            Some(41),
            "peer_inbox"
        ));
        assert!(!mark_push_subscribed(&ctx, true));
        assert!(!mark_push_subscribed(&ctx, true));
        assert!(mark_push_initialized_with(&ctx, |_| Ok(())));
        assert!(deliver_push_frame_with(
            &ctx,
            json!({"method": "test"}),
            None,
            "other",
            |_| Ok(())
        ));
        set_client_ready(&ctx, true);
        set_client_ready(&ctx, false);

        let records = read_debug_records(&path);
        let actions: Vec<_> = records
            .iter()
            .filter_map(|record| record["action"].as_str())
            .collect();
        assert_eq!(
            actions,
            [
                "push_frame_buffered",
                "push_subscribed",
                "push_initialized",
                "push_frame_emitted",
                "push_frame_emitted",
                "peer_set_ready_sent",
                "peer_set_ready_sent",
            ]
        );
        assert_eq!(records[0]["delivery_id"], 41);
        assert_eq!(records[0]["frame_kind"], "peer_inbox");
        assert_eq!(records[0]["pending_len_after"], 1);
        assert_eq!(records[1]["subscribed"], true);
        assert_eq!(records[1]["initialized_at_that_time"], false);
        assert_eq!(records[2]["flushed_count"], 1);
        assert_eq!(records[2]["subscribed_at_that_time"], true);
        assert_eq!(records[3]["via"], "initialized_flush");
        assert!(records[3]["initialized_age_ms"].is_number());
        assert_eq!(records[4]["via"], "direct");
        assert_eq!(records[5]["client_kind"], "codex");
        assert_eq!(records[5]["ready"], true);
        assert_eq!(records[5]["ok"], false);
        assert!(records[5]["error"].is_string());
        assert_eq!(records[6]["ready"], false);
        for record in records {
            assert_eq!(record["pane_id"], 1);
            assert!(record.get("process_id").is_some());
            assert!(record.get("record_sequence").is_some());
            assert!(record.get("timestamp_unix_ms").is_some());
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn push_debug_log_records_cap_drop_once() {
        let path = debug_test_path("push-cap");
        let ctx = connected_ctx_with_debug_log(path.clone());
        for n in 0..=PUSH_PENDING_CAP {
            deliver_push_frame_with(&ctx, json!({ "sequence": n }), None, "other", |_| Ok(()));
        }
        let records = read_debug_records(&path);
        let drops: Vec<_> = records
            .iter()
            .filter(|record| record["action"] == "push_frame_dropped_cap")
            .collect();
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0]["frame_kind"], "other");
        assert_eq!(drops[0]["delivery_id"], Value::Null);
        assert_eq!(drops[0]["pending_len"], PUSH_PENDING_CAP);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn push_debug_log_records_emit_failures_and_actual_flush_counts() {
        let path = debug_test_path("push-failures");
        let ctx = connected_ctx_with_debug_log(path.clone());
        deliver_push_frame(&ctx, json!({"sequence": 1}), Some(1), "peer_inbox");
        deliver_push_frame(&ctx, json!({"sequence": 2}), Some(2), "peer_inbox");
        let mut attempts = 0;
        mark_push_initialized_with(&ctx, |_| {
            attempts += 1;
            if attempts == 1 {
                Err(anyhow!("flush failed"))
            } else {
                Ok(())
            }
        });
        assert!(!deliver_push_frame_with(
            &ctx,
            json!({"sequence": 3}),
            Some(3),
            "peer_inbox",
            |_| Err(anyhow!("direct failed")),
        ));

        let records = read_debug_records(&path);
        let initialized = records
            .iter()
            .find(|record| record["action"] == "push_initialized")
            .expect("initialized record");
        assert_eq!(initialized["flushed_count"], 1);
        assert_eq!(initialized["failed_count"], 1);
        let failures: Vec<_> = records
            .iter()
            .filter(|record| record["action"] == "push_frame_emit_failed")
            .collect();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["delivery_id"], 1);
        assert_eq!(failures[0]["via"], "initialized_flush");
        assert_eq!(failures[0]["error"], "flush failed");
        assert!(failures[0]["initialized_age_ms"].is_number());
        assert_eq!(failures[1]["delivery_id"], 3);
        assert_eq!(failures[1]["via"], "direct");
        assert_eq!(failures[1]["error"], "direct failed");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn push_debug_log_disabled_writes_nothing_through_production_paths() {
        let _env_guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = debug_test_path("push-disabled");
        let _env_restore = EnvVarRestore::set(ENV_DEBUG_CODEX_PEER_LOG, path.as_os_str());
        let mut enabled = connected_ctx_with_debug_log(path.clone());
        deliver_push_frame(
            &enabled,
            json!({"method": "enabled"}),
            Some(1),
            "peer_inbox",
        );
        mark_push_subscribed(&enabled, true);
        mark_push_initialized_with(&enabled, |_| Ok(()));
        assert!(
            path.exists(),
            "enabled production paths must write the file"
        );
        std::fs::remove_file(&path).expect("remove enabled debug JSONL");

        enabled.debug_log_path = None;
        enabled.push = Arc::new(Mutex::new(PushState::default()));
        deliver_push_frame(
            &enabled,
            json!({"method": "disabled"}),
            Some(2),
            "peer_inbox",
        );
        mark_push_subscribed(&enabled, false);
        mark_push_initialized_with(&enabled, |_| Ok(()));
        assert!(!path.exists());
    }

    #[test]
    fn push_readiness_is_deferred_in_both_initialization_orders() {
        for initialized_first in [true, false] {
            let (ctx, requests) =
                connected_ctx_with_requests(PeerClientKind::Claude, Duration::from_millis(40));
            let initialized = json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            if initialized_first {
                dispatch(&initialized, &ctx).expect("initialized notification");
                publish_ready_after_subscribe(&ctx);
            } else {
                publish_ready_after_subscribe(&ctx);
                dispatch(&initialized, &ctx).expect("initialized notification");
            }
            assert!(
                requests.lock().unwrap().is_empty(),
                "push ready must not publish before the settling delay"
            );
            wait_for_request_count(&requests, 1);
            assert_eq!(ready_values(&requests), [true]);
        }
    }

    #[test]
    fn pull_readiness_is_published_immediately_after_subscribe() {
        let (ctx, requests) = connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        publish_ready_after_subscribe(&ctx);
        assert_eq!(ready_values(&requests), [true]);
    }

    #[test]
    fn push_ready_delay_is_measured_from_initialized_time() {
        let (ctx, _requests) =
            connected_ctx_with_requests(PeerClientKind::Claude, PUSH_READY_DELAY);
        let initialized_at = Instant::now();
        {
            let mut state = ctx.push.lock().unwrap();
            state.initialized = true;
            state.initialized_at = Some(initialized_at);
            state.subscribed = true;
        }
        let elapsed = Duration::from_millis(400);
        let deferred =
            prepare_deferred_push_ready_at(&ctx, initialized_at + elapsed).expect("ready deferral");
        assert_eq!(deferred.delay, PUSH_READY_DELAY - elapsed);
    }

    #[test]
    fn subscription_loss_cancels_deferred_true_and_publishes_false_immediately() {
        let (ctx, requests) =
            connected_ctx_with_requests(PeerClientKind::Claude, Duration::from_millis(40));
        mark_push_initialized_with(&ctx, |_| Ok(()));
        publish_ready_after_subscribe(&ctx);
        revoke_push_ready(&ctx);
        assert_eq!(ready_values(&requests), [false]);
        thread::sleep(Duration::from_millis(80));
        assert_eq!(ready_values(&requests), [false]);
    }

    #[test]
    fn repeated_initialized_notification_schedules_only_one_ready() {
        let (ctx, requests) =
            connected_ctx_with_requests(PeerClientKind::Claude, Duration::from_millis(20));
        publish_ready_after_subscribe(&ctx);
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        dispatch(&initialized, &ctx).expect("first initialized notification");
        dispatch(&initialized, &ctx).expect("duplicate initialized notification");
        wait_for_request_count(&requests, 1);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(ready_values(&requests), [true]);
    }

    #[test]
    fn deferred_ready_trace_records_delay_and_initialized_age() {
        let path = debug_test_path("ready-deferred");
        let (mut ctx, requests) =
            connected_ctx_with_requests(PeerClientKind::Claude, Duration::from_millis(20));
        ctx.debug_log_path = Some(path.clone());
        mark_push_initialized_with(&ctx, |_| Ok(()));
        publish_ready_after_subscribe(&ctx);
        wait_for_request_count(&requests, 1);
        let deadline = Instant::now() + Duration::from_secs(1);
        let records = loop {
            let records = read_debug_records(&path);
            if records
                .iter()
                .any(|record| record["action"] == "peer_set_ready_sent")
            {
                break records;
            }
            assert!(Instant::now() < deadline, "ready trace was not written");
            thread::sleep(Duration::from_millis(2));
        };
        let deferred = records
            .iter()
            .find(|record| record["action"] == "peer_set_ready_deferred")
            .expect("deferred trace");
        assert_eq!(
            records
                .iter()
                .filter(|record| record["action"] == "peer_set_ready_deferred")
                .count(),
            1
        );
        assert_eq!(deferred["client_kind"], "claude");
        assert!(deferred["delay_ms"].as_u64().is_some_and(|delay| delay > 0));
        let sent = records
            .iter()
            .find(|record| record["action"] == "peer_set_ready_sent")
            .expect("sent trace");
        assert_eq!(sent["ready"], true);
        assert!(sent["initialized_age_ms"].is_number());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn elapsed_delay_publishes_synchronously_without_deferred_trace() {
        let path = debug_test_path("ready-elapsed");
        let (mut ctx, requests) =
            connected_ctx_with_requests(PeerClientKind::Claude, Duration::ZERO);
        ctx.debug_log_path = Some(path.clone());
        mark_push_initialized_with(&ctx, |_| Ok(()));

        publish_ready_after_subscribe(&ctx);

        assert_eq!(ready_values(&requests), [true]);
        let records = read_debug_records(&path);
        assert!(records
            .iter()
            .all(|record| record["action"] != "peer_set_ready_deferred"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pre_initialized_push_buffer_is_bounded() {
        let ctx = connected_ctx_with(new_event_sink());
        for n in 0..(PUSH_PENDING_CAP + 1) {
            deliver_push_frame_with(&ctx, json!({ "sequence": n }), None, "other", |_| Ok(()));
        }
        let state = ctx.push.lock().unwrap();
        assert_eq!(state.pending.len(), PUSH_PENDING_CAP);
        assert_eq!(
            state.pending.front().map(|frame| &frame.value),
            Some(&json!({ "sequence": 0 }))
        );
        assert_eq!(
            state.pending.back().map(|frame| &frame.value),
            Some(&json!({ "sequence": PUSH_PENDING_CAP - 1 }))
        );
    }

    #[test]
    fn subscription_retry_backoff_doubles_and_caps() {
        assert_eq!(
            next_subscription_retry_delay(Duration::from_millis(250)),
            Duration::from_millis(500)
        );
        assert_eq!(
            next_subscription_retry_delay(Duration::from_secs(20)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_subscription_retry_delay(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn healthy_subscription_resets_retry_state() {
        assert_eq!(
            subscription_retry_state_after_attempt(
                9,
                Duration::from_secs(30),
                true,
                Duration::from_secs(30),
            ),
            (0, Duration::from_millis(250))
        );
        assert_eq!(
            subscription_retry_state_after_attempt(
                9,
                Duration::from_secs(30),
                true,
                Duration::from_secs(29),
            ),
            (9, Duration::from_secs(30))
        );
    }

    #[test]
    fn parse_target_defaults_to_focused_on_none() {
        assert!(matches!(parse_target(None), PaneRef::Focused));
    }

    #[test]
    fn parse_target_empty_string_is_focused() {
        assert!(matches!(parse_target(Some("")), PaneRef::Focused));
        assert!(matches!(parse_target(Some("   ")), PaneRef::Focused));
    }

    #[test]
    fn parse_target_focused_literal_is_case_insensitive() {
        assert!(matches!(parse_target(Some("focused")), PaneRef::Focused));
        assert!(matches!(parse_target(Some("FOCUSED")), PaneRef::Focused));
        assert!(matches!(parse_target(Some("Focused")), PaneRef::Focused));
    }

    #[test]
    fn parse_target_numeric_string_is_id() {
        match parse_target(Some("7")) {
            PaneRef::Id(n) => assert_eq!(n, 7),
            other => panic!("expected Id(7), got {other:?}"),
        }
        match parse_target(Some("  42  ")) {
            PaneRef::Id(n) => assert_eq!(n, 42),
            other => panic!("expected Id(42), got {other:?}"),
        }
    }

    #[test]
    fn parse_target_non_numeric_string_is_name() {
        match parse_target(Some("worker")) {
            PaneRef::Name(n) => assert_eq!(n, "worker"),
            other => panic!("expected Name, got {other:?}"),
        }
        // Names with digits mixed in stay as names, not ids.
        match parse_target(Some("worker-1")) {
            PaneRef::Name(n) => assert_eq!(n, "worker-1"),
            other => panic!("expected Name, got {other:?}"),
        }
    }

    #[test]
    fn parse_direction_maps_known_values() {
        assert!(matches!(
            parse_direction(Some("vertical")),
            Ok(Direction::Vertical)
        ));
        assert!(matches!(
            parse_direction(Some("horizontal")),
            Ok(Direction::Horizontal)
        ));
    }

    #[test]
    fn parse_direction_rejects_unknown_and_missing() {
        assert!(parse_direction(Some("diagonal")).is_err());
        assert!(parse_direction(None).is_err());
    }

    #[test]
    fn upgrade_claude_command_bare_claude_becomes_peer_enabled() {
        assert_eq!(upgrade_claude_command("claude"), CLAUDE_PEER_LAUNCH_CMD);
    }

    #[test]
    fn upgrade_claude_command_preserves_user_args_after_claude_token() {
        // `claude --resume` should keep `--resume` at the end; the
        // peer-channel flag is inserted right after the `claude` token.
        let got = upgrade_claude_command("claude --resume");
        assert_eq!(
            got,
            format!("{CLAUDE_PEER_LAUNCH_CMD} --resume"),
            "got {got:?}"
        );
    }

    #[test]
    fn upgrade_claude_command_noop_when_flag_already_present() {
        let already = "claude --dangerously-load-development-channels server:renga-peers --resume";
        assert_eq!(upgrade_claude_command(already), already);
        // A non-standard channel target the user may have hand-picked
        // must also pass through untouched.
        let custom = "claude --dangerously-load-development-channels server:other";
        assert_eq!(upgrade_claude_command(custom), custom);
    }

    #[test]
    fn upgrade_claude_command_ignores_non_claude_commands() {
        // The trigger is a whole-word `claude` at the start of the
        // first token only. `claude-mobile`, `claudex`, `./claude`,
        // and unrelated tools must pass through verbatim so we don't
        // rewrite a user script by accident.
        for input in [
            "cargo test",
            "claude-mobile --help",
            "claudex",
            "./claude",
            "env FOO=1 claude",
            "",
        ] {
            assert_eq!(
                upgrade_claude_command(input),
                input,
                "must not rewrite {input:?}"
            );
        }
    }

    #[test]
    fn upgrade_claude_command_preserves_leading_whitespace() {
        // Leading whitespace on the command (unusual but legal) is
        // preserved so indentation-sensitive shells don't get a
        // surprising rewrite.
        assert_eq!(
            upgrade_claude_command("  claude --resume"),
            format!("  {CLAUDE_PEER_LAUNCH_CMD} --resume")
        );
    }

    #[test]
    fn opt_string_trims_and_treats_empty_as_none() {
        let args = json!({ "a": "hi", "b": "  ", "c": "  padded  ", "d": 42 });
        assert_eq!(opt_string(&args, "a"), Some("hi".to_string()));
        assert_eq!(opt_string(&args, "b"), None);
        assert_eq!(opt_string(&args, "c"), Some("padded".to_string()));
        // Non-string values silently drop to None so Claude can't crash
        // the tool by passing an int where a string is expected.
        assert_eq!(opt_string(&args, "d"), None);
        assert_eq!(opt_string(&args, "missing"), None);
    }

    #[test]
    fn format_pane_list_empty() {
        assert_eq!(format_pane_list(&[]), "No panes in this tab.");
    }

    #[test]
    fn format_pane_list_includes_focus_and_geometry() {
        let panes = vec![
            PaneInfo {
                id: 1,
                pending_peer_messages: 3,
                name: Some("leader".into()),
                role: Some("foreman".into()),
                focused: true,
                x: 0,
                y: 0,
                width: 80,
                height: 24,
                cwd: None,
                kind: Some(PeerClientKind::Claude),
                receive_mode: Some(ipc::PeerReceiveMode::Push),
                summary: None,
            },
            PaneInfo {
                id: 2,
                pending_peer_messages: 0,
                name: None,
                role: None,
                focused: false,
                x: 80,
                y: 0,
                width: 40,
                height: 24,
                cwd: None,
                kind: Some(PeerClientKind::Codex),
                receive_mode: Some(ipc::PeerReceiveMode::Pull),
                summary: None,
            },
        ];
        let text = format_pane_list(&panes);
        assert!(text.contains("id=1"));
        assert!(text.contains("pending_peer_messages=3"));
        assert!(text.contains("name=leader"));
        assert!(text.contains("role=foreman"));
        assert!(text.contains("(focused)"));
        assert!(text.contains("width=80"));
        assert!(text.contains("id=2"));
        assert!(!text.contains("pending_peer_messages=0 name="));
    }

    #[test]
    fn format_peer_list_includes_pending_count() {
        let peers = vec![PeerInfo {
            id: 2,
            pending_peer_messages: 4,
            name: Some("worker".into()),
            role: None,
            cwd: None,
            kind: Some(PeerClientKind::Codex),
            receive_mode: Some(ipc::PeerReceiveMode::Pull),
            summary: None,
        }];

        let text = format_peer_list(&peers);
        assert!(text.contains("id=2 pending_peer_messages=4"), "{text}");
    }

    #[test]
    fn tools_spec_advertises_pane_control_tools() {
        let spec = tools_spec();
        let names: Vec<&str> = spec
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        for expected in [
            "list_peers",
            "send_message",
            "set_summary",
            "check_messages",
            "list_panes",
            "spawn_pane",
            "spawn_codex_pane",
            "close_pane",
            "focus_pane",
            "new_tab",
            "inspect_pane",
            "send_keys",
            "poll_events",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn spawn_pane_schema_requires_direction() {
        let spec = tools_spec();
        let spawn = spec
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("spawn_pane"))
            .expect("spawn_pane entry");
        let required = spawn
            .get("inputSchema")
            .and_then(|s| s.get("required"))
            .and_then(|r| r.as_array())
            .expect("required array");
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"direction"), "{required_names:?}");
    }

    #[test]
    fn tools_spec_advertises_set_pane_identity() {
        // Guard for issue #136: the rename API must appear in the MCP
        // tool list so Claude knows it exists without reading docs.
        let spec = tools_spec();
        let names: Vec<&str> = spec
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(
            names.contains(&"set_pane_identity"),
            "set_pane_identity missing from tools list: {names:?}"
        );
    }

    #[test]
    fn tools_spec_advertises_spawn_claude_pane() {
        // Guard for #137 — the higher-level Claude launcher must be
        // discoverable from tools/list so orchestrators find it
        // before falling back to spawn_pane(command=\"claude ...\").
        let spec = tools_spec();
        let names: Vec<&str> = spec
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(
            names.contains(&"spawn_claude_pane"),
            "spawn_claude_pane missing from tools list: {names:?}"
        );
    }

    #[test]
    fn tools_spec_advertises_spawn_codex_pane() {
        let spec = tools_spec();
        let names: Vec<&str> = spec
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(
            names.contains(&"spawn_codex_pane"),
            "spawn_codex_pane missing from tools list: {names:?}"
        );
    }

    #[test]
    fn spawn_tool_descriptions_report_queued_unconfirmed_startup() {
        let spec = tools_spec();
        let description = |name| {
            spec.as_array()
                .unwrap()
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
                .and_then(|tool| tool.get("description"))
                .and_then(Value::as_str)
                .expect("spawn tool description")
        };

        let spawn = description("spawn_pane");
        assert!(spawn.contains("queues any effective startup command"));
        assert!(spawn.contains("has not been confirmed started"));
        assert!(spawn.contains("`role` is exactly `claude`"));
        assert!(spawn.contains("for automatic execution"));
        assert!(spawn.contains("an explicit `command` takes precedence"));
        assert!(spawn.contains("Claude may take 90–150 s"));

        for name in ["spawn_claude_pane", "spawn_codex_pane"] {
            let description = description(name);
            assert!(description.contains("queues a"), "{name}: {description}");
            assert!(
                description.contains("startup is asynchronous and has not been confirmed"),
                "{name}: {description}"
            );
            assert!(
                description.contains("use `inspect_pane` to verify"),
                "{name}: {description}"
            );
        }

        for name in ["spawn_pane", "spawn_claude_pane", "spawn_codex_pane"] {
            let description = description(name);
            let lower = description.to_ascii_lowercase();
            assert!(!lower.contains("launches claude"), "{name}: {description}");
            assert!(!lower.contains("launches codex"), "{name}: {description}");
            assert!(!lower.contains("launches plain"), "{name}: {description}");
        }

        let role_description = spec
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("spawn_pane"))
            .and_then(|tool| tool.pointer("/inputSchema/properties/role/description"))
            .and_then(Value::as_str)
            .expect("spawn_pane role description");
        assert!(role_description.contains("`role` is exactly `claude`"));
        assert!(role_description.contains("automatic execution"));
        assert!(role_description.contains("an explicit `command` takes precedence"));

        for kind in [PeerClientKind::Claude, PeerClientKind::Codex] {
            let instructions = instructions_blob(kind);
            let lower = instructions.to_ascii_lowercase();
            assert!(!lower.contains("launches claude"), "{instructions}");
            assert!(!lower.contains("launches codex"), "{instructions}");
            assert!(!lower.contains("launches plain"), "{instructions}");
            assert!(
                !lower.contains("runs a startup command"),
                "instructions must describe command queueing: {instructions}"
            );
            assert!(instructions.contains("queues a startup command"));
            assert!(instructions.contains("Process startup is asynchronous"));
            assert!(instructions.contains("process startup is not confirmed"));
            assert!(instructions.contains("preserving all caller-provided trailing arguments"));
            let new_tab_instructions = instructions
                .split_once("- new_tab:")
                .and_then(|(_, tail)| tail.split_once("- inspect_pane:"))
                .map(|(section, _)| section)
                .expect("new_tab instructions section");
            assert!(new_tab_instructions.contains("queued for asynchronous execution"));
            assert!(new_tab_instructions.contains("process startup is not confirmed"));
            assert!(new_tab_instructions
                .contains("role `claude` queues the peer-enabled Claude command"));
            assert!(new_tab_instructions.contains("an explicit `command` takes precedence"));
        }
    }

    #[test]
    fn build_claude_launch_command_bare_defaults_to_peer_channel_only() {
        let got = build_claude_launch_command(None, None, &[]);
        assert_eq!(got, CLAUDE_PEER_LAUNCH_CMD);
    }

    #[test]
    fn build_codex_launch_command_bare_defaults_to_plain_codex() {
        let got = build_codex_launch_command(&[]);
        assert_eq!(got, "codex");
    }

    #[test]
    fn build_claude_launch_command_renders_permission_mode_and_model() {
        let got = build_claude_launch_command(Some("bypassPermissions"), Some("sonnet"), &[]);
        assert_eq!(
            got,
            format!("{CLAUDE_PEER_LAUNCH_CMD} --permission-mode bypassPermissions --model sonnet")
        );
    }

    #[test]
    fn build_claude_launch_command_appends_extra_args_after_structured() {
        let got = build_claude_launch_command(
            Some("auto"),
            None,
            &["--resume".to_string(), "--verbose".to_string()],
        );
        assert_eq!(
            got,
            format!("{CLAUDE_PEER_LAUNCH_CMD} --permission-mode auto --resume --verbose")
        );
    }

    #[test]
    fn build_claude_launch_command_always_includes_peer_channel_flag() {
        // Regression guard: any future refactor of the ordering must
        // keep the peer-channel flag at the front so Claude joins
        // renga-peers even when permission_mode / model are unset.
        // Also asserts the `--permission-mode bypassPermissions`
        // baseline (renga-234) survives independently of the
        // `CLAUDE_PEER_LAUNCH_CMD` constant — a substring check the
        // `format!("{CLAUDE_PEER_LAUNCH_CMD} ...")`-based tests can't
        // provide because they'd stay green if someone silently
        // dropped the flag from the constant.
        let got = build_claude_launch_command(None, None, &["--resume".to_string()]);
        assert!(
            got.contains("--dangerously-load-development-channels server:renga-peers"),
            "peer-channel flag missing: {got}"
        );
        assert!(
            got.contains("--permission-mode bypassPermissions"),
            "bypassPermissions baseline missing: {got}"
        );
    }

    #[test]
    fn validate_claude_extra_args_rejects_reserved_flags() {
        for bad in [
            "--dangerously-load-development-channels",
            "--permission-mode",
            "--model",
        ] {
            let err = validate_claude_extra_args(&[bad.to_string()], None)
                .expect_err("must reject reserved flag");
            assert!(
                err.contains(bad),
                "error must name the rejected flag: {err}"
            );
        }
    }

    #[test]
    fn validate_claude_extra_args_rejects_flag_equals_value_form() {
        // `--model=opus` shares the `--model` head, so the validator
        // must split on `=` and still reject. Otherwise a caller could
        // sneak a second --model past the structured field.
        let err = validate_claude_extra_args(&["--model=opus".to_string()], None)
            .expect_err("must reject --model=... form too");
        assert!(err.contains("--model"), "{err}");
    }

    #[test]
    fn validate_claude_extra_args_allows_unrelated_flags_when_allowlist_absent() {
        // Fall-open path: when soft validation can't fetch / parse
        // `claude --help`, any non-reserved flag passes through so a
        // missing or upgraded Claude binary never wedges the spawn.
        validate_claude_extra_args(
            &[
                "--resume".to_string(),
                "--verbose".to_string(),
                "/some-workflow".to_string(),
            ],
            None,
        )
        .expect("unrelated flags must be allowed when allowlist is absent");
    }

    #[test]
    fn shell_quote_passes_safe_chars_through() {
        assert_eq!(shell_quote("sonnet"), "sonnet");
        assert_eq!(shell_quote("bypassPermissions"), "bypassPermissions");
        assert_eq!(shell_quote("--resume"), "--resume");
        assert_eq!(shell_quote("/some-workflow"), "/some-workflow");
        assert_eq!(shell_quote("claude-opus-4-6"), "claude-opus-4-6");
        assert_eq!(shell_quote("a=b"), "a=b");
    }

    #[test]
    fn shell_quote_wraps_whitespace_in_single_quotes() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(
            shell_quote("C:/Program Files/claude"),
            "'C:/Program Files/claude'"
        );
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        // POSIX trick: close the quote, emit an escaped ', reopen.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_wraps_empty_string_so_no_arg_is_dropped() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_wraps_shell_metacharacters() {
        // `$`, `*`, `` ` ``, `;` etc must not be left bare — even if
        // no expansion target exists today, letting them through makes
        // the command re-parseable and breaks the "renga owns quoting"
        // contract that spawn_claude_pane documents.
        assert!(shell_quote("foo$bar").starts_with('\''));
        assert!(shell_quote("foo;bar").starts_with('\''));
        assert!(shell_quote("foo*").starts_with('\''));
        assert!(shell_quote("foo`bar").starts_with('\''));
    }

    #[test]
    fn build_claude_launch_command_quotes_values_with_whitespace() {
        // Regression guard for the Codex blocker: values with spaces
        // must not be re-split by the shell. A space-bearing
        // permission_mode or model or arg now round-trips as a single
        // shell token.
        let got = build_claude_launch_command(
            Some("accept edits"),
            Some("my model"),
            &["--config".to_string(), "C:/Program Files/foo".to_string()],
        );
        assert!(
            got.contains("--permission-mode 'accept edits'"),
            "permission_mode not quoted: {got}"
        );
        assert!(
            got.contains("--model 'my model'"),
            "model not quoted: {got}"
        );
        assert!(
            got.contains("'C:/Program Files/foo'"),
            "arg with space not quoted: {got}"
        );
    }

    #[test]
    fn build_codex_launch_command_quotes_values_with_whitespace() {
        let got = build_codex_launch_command(&[
            "--config".to_string(),
            "C:/Program Files/Codex".to_string(),
        ]);
        assert!(
            got.contains("'C:/Program Files/Codex'"),
            "arg with space not quoted: {got}"
        );
    }

    #[test]
    fn validate_claude_extra_args_does_not_reject_empty_head_boundary() {
        // `=oops` and `""` split into an empty head, which must not
        // match any reserved flag. Guard so a future refactor that
        // normalizes flag names can't accidentally treat "" as a
        // reserved match.
        validate_claude_extra_args(&["=oops".to_string(), String::new()], None)
            .expect("empty / no-head strings are not reserved flags");
    }

    /// Synthetic `claude --help` excerpt used by the parser and
    /// validator tests. Mirrors the structure renga sees in
    /// production: a Usage banner, an Options section with mixed
    /// short/long aliases, value placeholders (`<...>`, `[...]`),
    /// `--foo=value` documentation, and a Commands section that
    /// must be skipped.
    const SAMPLE_CLAUDE_HELP: &str = "\
Usage: claude [options] [command] [prompt]

Claude Code - starts an interactive session.

Arguments:
  prompt                                            Your prompt

Options:
  --add-dir <directories...>                        Additional directories
  --resume                                          Resume conversation
  -p, --print                                       Print and exit
  --model <model>                                   Model for the session
  --output-format <format>                          Output format (choices: \"text\", \"json\")
  --allowedTools, --allowed-tools <tools...>        Allowed tools list
  -d, --debug [filter]                              Enable debug mode
  --permission-mode <mode>                          Permission mode
  --dangerously-skip-permissions                    Skip permission checks
  -h, --help                                        Display help for command
  -v, --version                                     Output the version number
  -w, --worktree [name]                             Create a new git worktree

Commands:
  agents [options]                                  Manage agents
  doctor                                            Health check
  plugin|plugins                                    Manage plugins
";

    #[test]
    fn parse_claude_help_flags_extracts_long_and_short_forms() {
        let flags = parse_claude_help_flags(SAMPLE_CLAUDE_HELP);
        for expected in [
            "--add-dir",
            "--resume",
            "-p",
            "--print",
            "--model",
            "--output-format",
            "--allowedTools",
            "--allowed-tools",
            "-d",
            "--debug",
            "--permission-mode",
            "--dangerously-skip-permissions",
            "-h",
            "--help",
            "-v",
            "--version",
            "-w",
            "--worktree",
        ] {
            assert!(
                flags.contains(expected),
                "parser should extract {expected:?} from claude --help; got {flags:?}"
            );
        }
    }

    #[test]
    fn parse_claude_help_flags_skips_value_placeholders_and_subcommands() {
        let flags = parse_claude_help_flags(SAMPLE_CLAUDE_HELP);
        for noise in [
            "<directories...>",
            "<format>",
            "<tools...>",
            "<mode>",
            "[filter]",
            "[name]",
            "agents",
            "doctor",
            "plugin|plugins",
            "Usage:",
            "prompt",
            "claude",
            "Arguments:",
            "Options:",
            "Commands:",
            "-",
            "--",
        ] {
            assert!(
                !flags.contains(noise),
                "parser should skip {noise:?}; got {flags:?}"
            );
        }
    }

    #[test]
    fn parse_claude_help_flags_handles_empty_input() {
        // Defensive: a malformed or empty help dump must not panic
        // and must yield an empty allowlist (which the validator
        // then rejects every flag against — but the production path
        // treats parse failure as a fall-open via fetch_*_text).
        let flags = parse_claude_help_flags("");
        assert!(flags.is_empty());
    }

    #[test]
    fn validate_claude_extra_args_passes_known_flags_with_allowlist() {
        let allowlist = parse_claude_help_flags(SAMPLE_CLAUDE_HELP);
        validate_claude_extra_args(
            &[
                "--resume".to_string(),
                "--print".to_string(),
                "-d".to_string(),
                "--output-format=json".to_string(),
                "/some-workflow".to_string(),
                "Hello prompt".to_string(),
            ],
            Some(&allowlist),
        )
        .expect("known flags + positional values must pass when allowlist is present");
    }

    #[test]
    fn validate_claude_extra_args_rejects_unknown_flag_with_allowlist() {
        // Issue #229's motivating example: the dispatcher accidentally
        // forwarded `--skip-settings` (a flag that doesn't exist on
        // the Claude CLI). With the allowlist active this is now
        // rejected at the spawn boundary instead of failing later as
        // a Claude exit-1 inside the spawned pane.
        let allowlist = parse_claude_help_flags(SAMPLE_CLAUDE_HELP);
        let err = validate_claude_extra_args(&["--skip-settings".to_string()], Some(&allowlist))
            .expect_err("unknown flag must be rejected when allowlist is present");
        assert!(
            err.contains("--skip-settings"),
            "error must name the rejected flag: {err}"
        );
        assert!(
            err.contains("claude --help"),
            "error must reference the source of the allowlist: {err}"
        );
    }

    #[test]
    fn validate_claude_extra_args_rejects_unknown_flag_equals_value_form_with_allowlist() {
        // `--unknown=value` must also be rejected: the validator
        // splits on `=` and looks up the head, so `--unknown` is
        // checked against the allowlist regardless of whether the
        // caller used the equals-form or the space-separated form.
        let allowlist = parse_claude_help_flags(SAMPLE_CLAUDE_HELP);
        let err = validate_claude_extra_args(&["--unknown=value".to_string()], Some(&allowlist))
            .expect_err("--unknown=value form must also be rejected");
        assert!(err.contains("--unknown"), "{err}");
    }

    #[test]
    fn validate_claude_extra_args_passes_positional_args_with_allowlist() {
        // Non-flag args (prompts, file paths starting with `/`) must
        // pass through unconditionally — soft validation only gates
        // tokens that look like flags.
        let allowlist = parse_claude_help_flags(SAMPLE_CLAUDE_HELP);
        validate_claude_extra_args(
            &[
                "Hello, world!".to_string(),
                "/some-workflow".to_string(),
                "=oops".to_string(),
                String::new(),
            ],
            Some(&allowlist),
        )
        .expect("positional args must always pass through soft validation");
    }

    #[test]
    fn validate_claude_extra_args_reserved_flags_rejected_before_soft_check() {
        // Regression for issue #229: even with an allowlist that
        // happens to recognize the structured-field flags (which
        // SAMPLE_CLAUDE_HELP does for --model and --permission-mode),
        // the reserved-flag rejection MUST still fire so the caller
        // gets the structured-field nudge rather than a confusing
        // "unknown flag" error.
        let allowlist = parse_claude_help_flags(SAMPLE_CLAUDE_HELP);
        for bad in ["--model", "--permission-mode"] {
            let err = validate_claude_extra_args(&[bad.to_string()], Some(&allowlist))
                .expect_err("reserved flag must still be rejected with allowlist active");
            assert!(err.contains(bad), "{err}");
            assert!(
                err.contains("structured field"),
                "must mention structured field, got: {err}"
            );
        }
    }

    #[test]
    fn validate_claude_extra_args_falls_open_when_allowlist_is_none() {
        // The production fall-open path: `claude --help` failed (binary
        // missing, non-zero exit, etc.), so the caller passes None and
        // we accept any non-reserved flag. Mirrors pre-issue-#229
        // behavior so a missing Claude binary doesn't wedge spawning.
        validate_claude_extra_args(
            &[
                "--brand-new-flag".to_string(),
                "--probably-typo".to_string(),
            ],
            None,
        )
        .expect("must fall open when allowlist is None");
    }

    #[test]
    fn abbreviate_flag_list_truncates_long_lists() {
        let mut allowed = HashSet::new();
        for i in 0..20 {
            allowed.insert(format!("--flag-{i:02}"));
        }
        let rendered = abbreviate_flag_list(&allowed);
        assert!(rendered.contains("--flag-00"), "{rendered}");
        assert!(rendered.contains("8 more"), "{rendered}");
    }

    #[test]
    fn abbreviate_flag_list_inlines_short_lists() {
        let mut allowed = HashSet::new();
        allowed.insert("--resume".to_string());
        allowed.insert("--print".to_string());
        let rendered = abbreviate_flag_list(&allowed);
        assert!(rendered.contains("--resume"));
        assert!(rendered.contains("--print"));
        assert!(!rendered.contains("more"), "{rendered}");
    }

    #[test]
    fn spawn_claude_pane_accepts_empty_args_array() {
        // `args: []` is a legitimate "I have no extra args" payload —
        // the handler must not reject it, and must still call renga.
        // We can't reach the full IPC path without a server, so we
        // settle for: `build_claude_launch_command` handles empty
        // extra_args cleanly, mirroring what the handler forwards.
        let got = build_claude_launch_command(None, None, &[]);
        assert_eq!(got, CLAUDE_PEER_LAUNCH_CMD);
    }

    #[test]
    fn spawn_pane_success_response_reports_queued_not_started() {
        let response = spawn_pane_ok_response(
            &json!(41),
            &json!({ "id": 7, "startup_command": "cargo test" }),
            Some("ignored fallback"),
            false,
        );

        assert_eq!(
            response,
            json!({
                "jsonrpc": "2.0",
                "id": 41,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "Created pane id=7. Startup command queued (process start not yet confirmed; allow startup time, then use inspect_pane to verify): cargo test"
                    }],
                    "isError": false
                }
            })
        );
    }

    #[test]
    fn new_tab_handler_reports_server_confirmed_startup_command() {
        let ctx = connected_ctx_with(new_event_sink());
        let response = handle_new_tab_with_request(
            &json!(51),
            &json!({ "command": "cargo test" }),
            &ctx,
            |_, request| {
                assert!(matches!(
                    request,
                    Request::NewTab {
                        command: Some(command),
                        ..
                    } if command == "cargo test"
                ));
                Ok(Response::Ok {
                    data: json!({ "id": 12, "startup_command": "cargo test --locked" }),
                })
            },
        );

        assert_eq!(
            response,
            json!({
                "jsonrpc": "2.0",
                "id": 51,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "Opened new tab; new pane id=12 (now focused). Startup command queued (process start not yet confirmed; allow startup time, then use inspect_pane to verify): cargo test --locked"
                    }],
                    "isError": false
                }
            })
        );
    }

    #[test]
    fn new_tab_handler_reports_explicit_null_as_no_command() {
        let ctx = connected_ctx_with(new_event_sink());
        let response = handle_new_tab_with_request(&json!(52), &json!({}), &ctx, |_, _| {
            Ok(Response::Ok {
                data: json!({ "id": 13, "startup_command": null }),
            })
        });

        assert_eq!(
            response,
            json!({
                "jsonrpc": "2.0",
                "id": 52,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "Opened new tab; new pane id=13 (now focused). No startup command requested."
                    }],
                    "isError": false
                }
            })
        );
    }

    #[test]
    fn new_tab_without_command_or_role_needs_no_old_server_hedge() {
        let ctx = connected_ctx_with(new_event_sink());
        let response = handle_new_tab_with_request(&json!(55), &json!({}), &ctx, |_, _| {
            Ok(Response::Ok {
                data: json!({ "id": 15 }),
            })
        });

        assert_eq!(
            response,
            json!({
                "jsonrpc": "2.0",
                "id": 55,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "Opened new tab; new pane id=15 (now focused). No startup command requested."
                    }],
                    "isError": false
                }
            })
        );
    }

    #[test]
    fn new_tab_handler_hedges_missing_or_non_string_startup_command() {
        let ctx = connected_ctx_with(new_event_sink());
        for (id, data) in [
            (53, json!({ "id": 14 })),
            (
                54,
                json!({
                    "id": 14,
                    "startup_command": 42
                }),
            ),
        ] {
            let response = handle_new_tab_with_request(
                &json!(id),
                &json!({ "command": "cargo test" }),
                &ctx,
                |_, _| Ok(Response::Ok { data }),
            );

            assert_eq!(
                response,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": "Opened new tab; new pane id=14 (now focused). Startup command unconfirmed (renga server may predate effective-command reporting; process start not yet confirmed; allow startup time, then use inspect_pane to verify): cargo test"
                        }],
                        "isError": false
                    }
                })
            );
        }
    }

    #[test]
    fn spawn_pane_success_response_without_command_reports_none_requested() {
        let response = spawn_pane_ok_response(
            &json!(42),
            &json!({ "id": 7, "startup_command": null }),
            None,
            false,
        );

        assert_eq!(
            response,
            json!({
                "jsonrpc": "2.0",
                "id": 42,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "Created pane id=7. No startup command requested."
                    }],
                    "isError": false
                }
            })
        );
    }

    #[test]
    fn spawn_pane_missing_startup_command_is_unconfirmed_for_older_server() {
        let response =
            spawn_pane_ok_response(&json!(44), &json!({ "id": 7 }), Some("cargo test"), false);
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text result");

        assert_eq!(
            text,
            "Created pane id=7. Startup command unconfirmed (renga server may predate effective-command reporting; process start not yet confirmed; allow startup time, then use inspect_pane to verify): cargo test"
        );
        assert!(!text.contains("No startup command requested."));
    }

    #[test]
    fn spawn_pane_non_string_startup_command_is_unconfirmed() {
        let response = spawn_pane_ok_response(
            &json!(45),
            &json!({ "id": 7, "startup_command": 42 }),
            None,
            false,
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text result");

        assert!(text.contains("Startup command unconfirmed"), "{text}");
        assert!(!text.contains("No startup command requested."));
    }

    #[test]
    fn spawn_pane_without_command_or_role_needs_no_old_server_hedge() {
        let response = spawn_pane_ok_response(&json!(46), &json!({ "id": 7 }), None, true);
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text result");

        assert_eq!(text, "Created pane id=7. No startup command requested.");
    }

    #[test]
    fn spawn_claude_pane_success_response_reports_queued_not_started() {
        let response = spawn_claude_pane_ok_response(
            &json!(43),
            &json!({ "id": 9, "startup_command": "claude --model sonnet" }),
            "claude --model opus",
        );

        assert_eq!(
            response,
            json!({
                "jsonrpc": "2.0",
                "id": 43,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "Created pane id=9. Startup command queued for Claude (process start not yet confirmed; allow startup time, then use inspect_pane to verify; Claude may take 90–150 s): claude --model sonnet"
                    }],
                    "isError": false
                }
            })
        );
    }

    #[test]
    fn specialized_spawn_responses_hedge_with_local_command_for_older_server() {
        for (response, expected) in [
            (
                spawn_claude_pane_ok_response(
                    &json!(46),
                    &json!({ "id": 9 }),
                    "claude --model opus",
                ),
                "Created pane id=9. Startup command unconfirmed for Claude (renga server may predate effective-command reporting; process start not yet confirmed; allow startup time, then use inspect_pane to verify; Claude may take 90–150 s): claude --model opus",
            ),
            (
                spawn_codex_pane_ok_response(
                    &json!(47),
                    &json!({ "id": 10 }),
                    "codex --yolo",
                ),
                "Created pane id=10. Startup command unconfirmed for Codex (renga server may predate effective-command reporting; process start not yet confirmed; allow startup time, then use inspect_pane to verify; Codex may take 90–150 s): codex --yolo",
            ),
        ] {
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .expect("text result");
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn spawn_claude_pane_rejects_null_args_as_invalid_params() {
        // `args: null` is not a missing key — it's explicitly present
        // with a null value, which the schema disallows. The handler
        // must return -32602, not silently treat it as "no args".
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        let resp =
            handle_spawn_claude_pane(&id, &json!({ "direction": "vertical", "args": null }), &ctx);
        let err_code = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64());
        assert_eq!(err_code, Some(-32602), "resp={resp}");
    }

    #[test]
    fn spawn_claude_pane_rejects_non_array_args() {
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        let resp = handle_spawn_claude_pane(
            &id,
            &json!({ "direction": "vertical", "args": "not-an-array" }),
            &ctx,
        );
        let err_code = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64());
        assert_eq!(err_code, Some(-32602), "resp={resp}");
    }

    #[test]
    fn spawn_claude_pane_rejects_missing_direction() {
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        let resp = handle_spawn_claude_pane(&id, &json!({}), &ctx);
        let err_code = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64());
        assert_eq!(err_code, Some(-32602), "resp={resp}");
    }

    #[test]
    fn spawn_claude_pane_rejects_reserved_flag_in_args() {
        // End-to-end: the dispatcher must catch reserved flags before
        // touching renga IPC, so the rejection happens even when the
        // server is fully reachable.
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        let resp = handle_spawn_claude_pane(
            &id,
            &json!({
                "direction": "vertical",
                "args": ["--permission-mode", "plan"]
            }),
            &ctx,
        );
        let err_code = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64());
        assert_eq!(err_code, Some(-32602), "resp={resp}");
    }

    #[test]
    fn spawn_codex_pane_rejects_null_args_as_invalid_params() {
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        let resp =
            handle_spawn_codex_pane(&id, &json!({ "direction": "vertical", "args": null }), &ctx);
        let err_code = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64());
        assert_eq!(err_code, Some(-32602), "resp={resp}");
    }

    #[test]
    fn spawn_codex_pane_success_response_uses_server_command() {
        let response = spawn_codex_pane_ok_response(
            &json!(48),
            &json!({ "id": 8, "startup_command": "codex --model gpt-5" }),
            "codex --yolo",
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text result");

        assert_eq!(
            text,
            "Created pane id=8. Startup command queued for Codex (process start not yet confirmed; allow startup time, then use inspect_pane to verify; Codex may take 90–150 s): codex --model gpt-5"
        );
    }

    #[test]
    fn spawn_codex_pane_errors_when_codex_install_missing() {
        // Issue #203: when ~/.codex/config.toml does not declare
        // `RENGA_PEER_CLIENT_KIND=codex` for the renga-peers MCP entry,
        // the spawned codex pane would otherwise register as a `claude`
        // (push) client. The handler must short-circuit with the
        // `[codex_not_installed]` marker pointing at
        // `renga mcp install --client codex`.
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        fn verify_unset() -> std::result::Result<(), String> {
            Err("RENGA_PEER_CLIENT_KIND not set in Codex MCP config".to_string())
        }
        let resp = handle_spawn_codex_pane_with(
            &id,
            &json!({ "direction": "vertical" }),
            &ctx,
            verify_unset,
        );
        let err = resp.get("error").expect("error envelope");
        assert_eq!(err.get("code").and_then(|c| c.as_i64()), Some(-32603));
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        assert!(
            msg.contains("[codex_not_installed]"),
            "message missing error code: {msg}"
        );
        assert!(
            msg.contains("renga mcp install --client codex"),
            "message missing remediation hint: {msg}"
        );
    }

    #[test]
    fn spawn_codex_pane_proceeds_when_codex_install_verified() {
        // Sanity: a passing verifier must not short-circuit before
        // the regular `require_connected` / Split flow. The test ctx
        // points at a non-existent endpoint, so the call ultimately
        // fails with -32603 from `client::send_request`, but it must
        // *not* be the `[codex_not_installed]` short-circuit.
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        fn verify_ok() -> std::result::Result<(), String> {
            Ok(())
        }
        let resp =
            handle_spawn_codex_pane_with(&id, &json!({ "direction": "vertical" }), &ctx, verify_ok);
        let msg = resp
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        assert!(
            !msg.contains("[codex_not_installed]"),
            "verify_ok must not surface codex_not_installed: {msg}"
        );
    }

    #[test]
    fn spawn_codex_pane_rejects_missing_direction() {
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        let resp = handle_spawn_codex_pane(&id, &json!({}), &ctx);
        let err_code = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64());
        assert_eq!(err_code, Some(-32602), "resp={resp}");
    }

    #[test]
    fn set_pane_identity_rejects_empty_payload() {
        // MCP-side guard: calling set_pane_identity with neither
        // `name` nor `role` must return an invalid-params error so
        // typo'd payloads (`nmae`) don't silently succeed.
        let ctx = connected_ctx_with(Arc::new((
            Mutex::new(EventBuffer::default()),
            Condvar::new(),
        )));
        let id = json!(1);
        let resp = handle_set_pane_identity(&id, &json!({ "target": "focused" }), &ctx);
        let error_code = resp
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64());
        assert_eq!(error_code, Some(-32602), "resp={resp}");
    }

    #[test]
    fn spawn_pane_and_new_tab_schemas_advertise_cwd() {
        // Regression guard for issue #135: callers must see `cwd` as
        // an optional property on both pane-creation tools so they
        // can stop embedding `cd <dir> &&` in `command`.
        let spec = tools_spec();
        for tool in ["spawn_pane", "new_tab"] {
            let entry = spec
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(tool))
                .unwrap_or_else(|| panic!("{tool} entry"));
            let props = entry
                .get("inputSchema")
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.as_object())
                .unwrap_or_else(|| panic!("{tool} properties"));
            assert!(
                props.contains_key("cwd"),
                "{tool} schema must advertise cwd property"
            );
        }
    }

    #[test]
    fn new_tab_schema_explains_queued_command_and_claude_role_default() {
        let spec = tools_spec();
        let new_tab = spec
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool.get("name").and_then(|value| value.as_str()) == Some("new_tab"))
            .expect("new_tab entry");
        let properties = &new_tab["inputSchema"]["properties"];

        assert_eq!(
            properties["command"]["description"],
            "Optional shell command to queue in the new pane. Process start is asynchronous and not yet confirmed when new_tab returns; allow startup time, then use inspect_pane to verify. A bare `claude` (or `claude <args>`) is auto-upgraded to the Alt+P peer-enabled form so the new instance joins the renga-peers network. If you pass --dangerously-load-development-channels explicitly, it is left alone."
        );
        assert_eq!(
            properties["role"]["description"],
            "Optional free-form role label attached to the new pane. When command is omitted and role is exactly `claude`, renga queues the peer-enabled Claude startup command; an explicit command takes precedence. Process start is not confirmed by the response."
        );
    }

    #[test]
    fn close_pane_schema_requires_target() {
        let spec = tools_spec();
        let close = spec
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("close_pane"))
            .expect("close_pane entry");
        let required: Vec<&str> = close
            .get("inputSchema")
            .and_then(|s| s.get("required"))
            .and_then(|r| r.as_array())
            .expect("required array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&"target"), "{required:?}");
    }

    #[test]
    fn detached_mode_surfaces_friendly_text_instead_of_error() {
        // When RENGA_PANE_ID/RENGA_SOCKET are missing, pane-control
        // tools must still return a Response::Ok with explanatory text
        // rather than a JSON-RPC error, so Claude can relay the reason
        // to the user instead of treating the tool as broken.
        let ctx = detached_ctx("RENGA_PANE_ID not set");
        let id = json!(1);
        let resp = handle_list_panes(&id, &ctx);
        assert_eq!(
            resp.get("result")
                .and_then(|r| r.get("isError"))
                .and_then(|v| v.as_bool()),
            Some(false),
            "expected Ok result, got {resp}"
        );
        let text = resp
            .pointer("/result/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            text.contains("renga not reachable"),
            "missing explanation in {text:?}"
        );
    }

    #[test]
    fn close_pane_rejects_empty_target_argument() {
        // Even with a live ctx, close_pane must refuse an empty target
        // at the tool layer without round-tripping to renga, so
        // Claude gets an immediate JSON-RPC -32602 it can retry with a
        // real id.
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        let resp = handle_close_pane(&id, &json!({ "target": "   " }), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params error, got {resp}"
        );
    }

    #[test]
    fn focus_pane_rejects_empty_target_argument() {
        // Parallel to `close_pane_rejects_empty_target_argument`. A
        // regression here would let a bare `focus_pane` call silently
        // resolve to `PaneRef::Focused`, focusing the caller on itself
        // instead of erroring on missing input.
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        let resp = handle_focus_pane(&id, &json!({ "target": "" }), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params error, got {resp}"
        );
    }

    #[test]
    fn spawn_pane_rejects_missing_direction() {
        // `spawn_pane` validates direction before touching renga, so a
        // missing or unknown value must come back as -32602 even when
        // no server is reachable.
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        let resp = handle_spawn_pane(&id, &json!({}), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params error, got {resp}"
        );
        let resp = handle_spawn_pane(&id, &json!({ "direction": "diagonal" }), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params error for bad direction, got {resp}"
        );
    }

    #[test]
    fn parse_target_overflow_and_negative_fall_back_to_name() {
        // Documented behavior: strings that look numeric but can't be
        // represented as usize (overflow, leading `-`) drop to
        // `PaneRef::Name` rather than erroring. The server will return
        // `pane_not_found` either way; the point of this test is to
        // freeze the fallthrough so a refactor to a fallible
        // `parse_target` has to revisit every caller.
        let overflow = "99999999999999999999999999999999";
        match parse_target(Some(overflow)) {
            PaneRef::Name(n) => assert_eq!(n, overflow),
            other => panic!("expected Name on overflow, got {other:?}"),
        }
        match parse_target(Some("-1")) {
            PaneRef::Name(n) => assert_eq!(n, "-1"),
            other => panic!("expected Name for negative, got {other:?}"),
        }
        // Leading `+` is accepted by `usize::from_str` in the stdlib,
        // so "+3" parses cleanly as Id(3). Pin that quirk here so a
        // future "strictly all-digit" rewrite notices it.
        assert!(matches!(parse_target(Some("+3")), PaneRef::Id(3)));
    }

    #[test]
    fn parse_target_pins_digit_string_to_id_not_name() {
        // Pin the documented behavior: any all-digit string resolves
        // to PaneRef::Id, even if the user meant a pane literally
        // named "7". Tool descriptions warn about this; this test
        // guards against someone "fixing" the ambiguity by checking
        // for a matching name first.
        assert!(matches!(parse_target(Some("7")), PaneRef::Id(7)));
        assert!(matches!(parse_target(Some("0")), PaneRef::Id(0)));
        // Names starting with a digit but containing non-digits stay
        // as names (so "7worker" is still addressable).
        match parse_target(Some("7worker")) {
            PaneRef::Name(n) => assert_eq!(n, "7worker"),
            other => panic!("expected Name(\"7worker\"), got {other:?}"),
        }
    }

    // ── inspect_pane unit tests ───────────────────────────────

    #[test]
    fn parse_inspect_format_defaults_to_text() {
        assert_eq!(parse_inspect_format(None), Ok(InspectFormat::Text));
        assert_eq!(parse_inspect_format(Some("")), Ok(InspectFormat::Text));
        assert_eq!(parse_inspect_format(Some("  ")), Ok(InspectFormat::Text));
        assert_eq!(parse_inspect_format(Some("text")), Ok(InspectFormat::Text));
    }

    #[test]
    fn parse_inspect_format_accepts_grid() {
        assert_eq!(parse_inspect_format(Some("grid")), Ok(InspectFormat::Grid));
    }

    #[test]
    fn parse_inspect_format_rejects_unknown() {
        assert!(parse_inspect_format(Some("json")).is_err());
        assert!(parse_inspect_format(Some("GRID")).is_err());
    }

    #[test]
    fn inspect_text_block_returns_text_field() {
        let payload = json!({
            "text": "line1\nline2",
            "lines": [{ "row": 0, "text": "line1" }],
        });
        assert_eq!(inspect_text_block(&payload), "line1\nline2");
    }

    #[test]
    fn inspect_text_block_returns_empty_on_missing_field() {
        // A malformed payload without `text` must not panic — callers
        // rely on the tool never crashing the MCP dispatcher even when
        // the inspect response shape regresses.
        let payload = json!({ "lines": [] });
        assert_eq!(inspect_text_block(&payload), "");
    }

    #[test]
    fn inspect_grid_block_renders_lines_as_pretty_json() {
        let payload = json!({
            "lines": [
                { "row": 0, "text": "hello" },
                { "row": 1, "text": "world" },
            ],
            "text": "hello\nworld",
        });
        let out = inspect_grid_block(&payload);
        // Pretty-printed JSON starts with `[` on its own line and
        // contains each row's text.
        assert!(out.starts_with('['), "expected JSON array, got {out:?}");
        assert!(out.contains("\"hello\""), "missing line text: {out}");
        assert!(out.contains("\"world\""), "missing line text: {out}");
    }

    #[test]
    fn inspect_grid_block_falls_back_to_text_when_lines_missing() {
        // Forward-compat: if a future renga server returns only `text`
        // without `lines`, we still surface something useful instead of
        // an empty string that looks like "nothing to see".
        let payload = json!({ "text": "only-text" });
        assert_eq!(inspect_grid_block(&payload), "only-text");
    }

    #[test]
    fn handle_inspect_pane_rejects_empty_target() {
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        let resp = handle_inspect_pane(&id, &json!({ "target": "   " }), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params error, got {resp}"
        );
    }

    #[test]
    fn handle_inspect_pane_rejects_unknown_format() {
        // Format validation runs before any IPC round-trip, so a bad
        // `format` must come back as -32602 even in detached mode.
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        let resp = handle_inspect_pane(&id, &json!({ "target": "1", "format": "csv" }), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params error for bad format, got {resp}"
        );
    }

    #[test]
    fn handle_inspect_pane_detached_surfaces_friendly_text() {
        // Detached mode must not error; instead return the standard
        // "renga not reachable" text so Claude can relay it to the user.
        let ctx = detached_ctx("RENGA_PANE_ID not set");
        let id = json!(1);
        let resp = handle_inspect_pane(&id, &json!({ "target": "1" }), &ctx);
        let text = resp
            .pointer("/result/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            text.contains("renga not reachable"),
            "missing explanation in {text:?}"
        );
    }

    #[test]
    fn inspect_pane_schema_requires_target() {
        // Pin the Issue #116 contract: the tool schema must enforce
        // `target` as required so Claude can't call without it.
        let spec = tools_spec();
        let inspect = spec
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("inspect_pane"))
            .expect("inspect_pane entry");
        let required: Vec<&str> = inspect
            .get("inputSchema")
            .and_then(|s| s.get("required"))
            .and_then(|r| r.as_array())
            .expect("required array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["target"], "{required:?}");
    }

    // ── send_keys unit tests ──────────────────────────────────

    #[test]
    fn translate_key_maps_common_named_keys() {
        assert_eq!(translate_key("Enter").as_deref(), Some("\r"));
        assert_eq!(translate_key("Return").as_deref(), Some("\r"));
        assert_eq!(translate_key("Tab").as_deref(), Some("\t"));
        assert_eq!(translate_key("Shift+Tab").as_deref(), Some("\x1b[Z"));
        assert_eq!(translate_key("BackTab").as_deref(), Some("\x1b[Z"));
        assert_eq!(translate_key("Esc").as_deref(), Some("\x1b"));
        assert_eq!(translate_key("Escape").as_deref(), Some("\x1b"));
        assert_eq!(translate_key("Backspace").as_deref(), Some("\x7f"));
        assert_eq!(translate_key("Delete").as_deref(), Some("\x1b[3~"));
        assert_eq!(translate_key("Up").as_deref(), Some("\x1b[A"));
        assert_eq!(translate_key("Down").as_deref(), Some("\x1b[B"));
        assert_eq!(translate_key("Right").as_deref(), Some("\x1b[C"));
        assert_eq!(translate_key("Left").as_deref(), Some("\x1b[D"));
        assert_eq!(translate_key("Space").as_deref(), Some(" "));
    }

    #[test]
    fn translate_key_trims_whitespace() {
        assert_eq!(translate_key("  Enter  ").as_deref(), Some("\r"));
    }

    #[test]
    fn translate_key_handles_ctrl_letter_case_insensitively() {
        assert_eq!(translate_key("Ctrl+C").as_deref(), Some("\x03"));
        assert_eq!(translate_key("Ctrl+c").as_deref(), Some("\x03"));
        assert_eq!(translate_key("Ctrl+A").as_deref(), Some("\x01"));
        assert_eq!(translate_key("Ctrl+Z").as_deref(), Some("\x1a"));
    }

    #[test]
    fn translate_key_rejects_unknown_and_malformed_ctrl() {
        assert_eq!(translate_key("Foo"), None);
        assert_eq!(translate_key("Ctrl+"), None);
        assert_eq!(translate_key("Ctrl+AB"), None);
        assert_eq!(translate_key("Ctrl+1"), None);
        assert_eq!(translate_key(""), None);
    }

    #[test]
    fn build_send_keys_payload_combines_text_keys_and_enter() {
        // Enter is CR (0x0D), not LF, because raw-mode TUIs read bytes
        // directly from the PTY.
        let keys = vec![Value::String("Enter".to_string())];
        let out = build_send_keys_payload("y", Some(&keys), false).unwrap();
        assert_eq!(out, "y\r");

        let out = build_send_keys_payload("y", None, true).unwrap();
        assert_eq!(out, "y\r");

        let keys = vec![Value::String("Shift+Tab".to_string())];
        let out = build_send_keys_payload("", Some(&keys), false).unwrap();
        assert_eq!(out, "\x1b[Z");
    }

    #[test]
    fn build_send_keys_payload_rejects_empty_input() {
        let err = build_send_keys_payload("", None, false).unwrap_err();
        assert!(err.contains("at least one"), "{err}");

        let err = build_send_keys_payload("", Some(&[]), false).unwrap_err();
        assert!(err.contains("at least one"), "{err}");
    }

    #[test]
    fn build_send_keys_payload_rejects_unknown_key() {
        let keys = vec![Value::String("Hyper+Meta".to_string())];
        let err = build_send_keys_payload("", Some(&keys), false).unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
        assert!(err.contains("Hyper+Meta"), "{err}");
    }

    #[test]
    fn build_send_keys_payload_rejects_non_string_key() {
        let keys = vec![Value::Number(42.into())];
        let err = build_send_keys_payload("", Some(&keys), false).unwrap_err();
        assert!(err.contains("must be strings"), "{err}");
    }

    #[test]
    fn handle_send_keys_rejects_empty_target() {
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        let resp = handle_send_keys(&id, &json!({ "target": "   ", "text": "y" }), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params, got {resp}"
        );
    }

    // ── poll_events unit tests ────────────────────────────────

    fn dummy_endpoint() -> crate::ipc::endpoint::EndpointName {
        // Cross-platform dummy endpoint constructor for tests that
        // only need a Connected mode — the poll_events handler never
        // opens the endpoint because it reads from the in-process
        // EventSink, so the actual value doesn't matter.
        #[cfg(windows)]
        {
            crate::ipc::endpoint::EndpointName::pipe("renga-test-endpoint")
        }
        #[cfg(unix)]
        {
            crate::ipc::endpoint::EndpointName::socket(std::path::PathBuf::from(
                "renga-test-endpoint",
            ))
        }
    }

    fn detached_ctx(reason: &str) -> PeerCtx {
        PeerCtx {
            mode: Mode::Detached {
                reason: reason.to_string(),
            },
            client_kind: PeerClientKind::Claude,
            events: new_event_sink(),
            inbox: new_inbox_sink(),
            push: Arc::new(Mutex::new(PushState::default())),
            ready_publish_lock: Arc::new(Mutex::new(())),
            peer_inbox_request_sender: Arc::new(Mutex::new(PeerInboxRequestRoute::Unavailable)),
            unreported_consumed_overflow: Arc::new(AtomicUsize::new(0)),
            unreported_consumed: Arc::new(Mutex::new(VecDeque::new())),
            debug_log_path: None,
            request_sink: None,
            request_sink_response: None,
            reconcile_snapshot_barrier: None,
            push_ready_delay: PUSH_READY_DELAY,
        }
    }

    fn connected_ctx_with_kind(events: EventSink, client_kind: PeerClientKind) -> PeerCtx {
        PeerCtx {
            mode: Mode::Connected {
                pane_id: 1,
                endpoint: dummy_endpoint(),
            },
            client_kind,
            events,
            inbox: new_inbox_sink(),
            push: Arc::new(Mutex::new(PushState::default())),
            ready_publish_lock: Arc::new(Mutex::new(())),
            peer_inbox_request_sender: Arc::new(Mutex::new(PeerInboxRequestRoute::Unavailable)),
            unreported_consumed_overflow: Arc::new(AtomicUsize::new(0)),
            unreported_consumed: Arc::new(Mutex::new(VecDeque::new())),
            debug_log_path: None,
            request_sink: None,
            request_sink_response: None,
            reconcile_snapshot_barrier: None,
            push_ready_delay: PUSH_READY_DELAY,
        }
    }

    fn connected_ctx_with(events: EventSink) -> PeerCtx {
        connected_ctx_with_kind(events, PeerClientKind::Claude)
    }

    fn connected_ctx_with_requests(
        client_kind: PeerClientKind,
        push_ready_delay: Duration,
    ) -> (PeerCtx, Arc<Mutex<Vec<Request>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = connected_ctx_with_kind(new_event_sink(), client_kind);
        ctx.request_sink = Some(requests.clone());
        ctx.request_sink_response = Some(Response::ok_unit());
        ctx.push_ready_delay = push_ready_delay;
        (ctx, requests)
    }

    fn wait_for_request_count(requests: &Arc<Mutex<Vec<Request>>>, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while requests.lock().unwrap().len() < expected && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(requests.lock().unwrap().len(), expected);
    }

    fn ready_values(requests: &Arc<Mutex<Vec<Request>>>) -> Vec<bool> {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| match request {
                Request::PeerSetReady { ready, .. } => Some(*ready),
                _ => None,
            })
            .collect()
    }

    fn connected_ctx_with_debug_log(path: PathBuf) -> PeerCtx {
        let mut ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        ctx.debug_log_path = Some(path);
        ctx
    }

    fn debug_test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "renga-mcp-peer-{label}-{}-{}.jsonl",
            std::process::id(),
            PEER_DEBUG_RECORD_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn failed_peer_trace_append_is_reported_by_next_record() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        PEER_DEBUG_WRITE_FAILURES.store(0, std::sync::atomic::Ordering::Relaxed);
        append_peer_debug_record(
            &std::env::temp_dir(),
            Some(1),
            json!({"action": "will_fail"}),
        );
        assert_eq!(
            PEER_DEBUG_WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        let path = debug_test_path("write-failure");
        append_peer_debug_record(&path, Some(1), json!({"action": "check_messages"}));
        let records = read_debug_records(&path);
        assert_eq!(records[0]["trace_write_failures_since_last"], 1);
        assert_eq!(records[0]["component"], "mcp_peer");
        assert_eq!(
            PEER_DEBUG_WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_peer_trace_write_all_is_counted() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        PEER_DEBUG_WRITE_FAILURES.store(2, std::sync::atomic::Ordering::Relaxed);
        let reported = take_peer_debug_write_failures();
        let result = FailingWriter.write_all(b"record");
        record_peer_write_result(reported, result);
        assert_eq!(
            PEER_DEBUG_WRITE_FAILURES.swap(0, std::sync::atomic::Ordering::AcqRel),
            3
        );
    }

    #[test]
    fn concurrent_peer_trace_failure_takes_report_each_failure_once() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        PEER_DEBUG_WRITE_FAILURES.store(5, std::sync::atomic::Ordering::Relaxed);
        let first = thread::spawn(take_peer_debug_write_failures);
        let second = thread::spawn(take_peer_debug_write_failures);
        let reported = first.join().unwrap() + second.join().unwrap();
        assert_eq!(reported, 5);
        assert_eq!(take_peer_debug_write_failures(), 0);
    }

    fn read_debug_records(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("debug log")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSONL record"))
            .collect()
    }

    fn pane_exited_value(id: usize, seq_ts: u64) -> Value {
        json!({
            "type": "pane_exited",
            "id": id,
            "ts_ms": seq_ts,
        })
    }

    fn pane_started_value(id: usize, seq_ts: u64) -> Value {
        json!({
            "type": "pane_started",
            "id": id,
            "ts_ms": seq_ts,
        })
    }

    fn structured(resp: &Value) -> &Value {
        resp.pointer("/result/structuredContent")
            .expect("structuredContent")
    }

    #[test]
    fn event_buffer_assigns_monotonic_one_based_seqs() {
        let mut buf = EventBuffer::default();
        let a = buf.push(pane_started_value(1, 10));
        let b = buf.push(pane_exited_value(1, 20));
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(buf.last_seq, 2);
        assert_eq!(buf.events.len(), 2);
    }

    #[test]
    fn event_buffer_evicts_oldest_beyond_cap() {
        let mut buf = EventBuffer::default();
        for i in 0..(EVENT_BUFFER_CAP + 5) {
            buf.push(pane_started_value(i, i as u64));
        }
        assert_eq!(buf.events.len(), EVENT_BUFFER_CAP);
        let first = buf.events.front().unwrap().seq;
        let last = buf.events.back().unwrap().seq;
        assert_eq!(first, 6);
        assert_eq!(last, (EVENT_BUFFER_CAP + 5) as u64);
    }

    #[test]
    fn scan_buffer_empty_window_returns_none() {
        let buf = EventBuffer::default();
        let scan = scan_buffer(&buf, 1, None);
        assert_eq!(
            scan,
            PollScan {
                matched: Vec::new(),
                window_max_seq: None
            }
        );
    }

    #[test]
    fn handle_send_keys_rejects_unknown_key_name_before_ipc() {
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        let resp = handle_send_keys(&id, &json!({ "target": "1", "keys": ["Nonsense"] }), &ctx);
        assert_eq!(
            resp.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32602),
            "expected invalid-params, got {resp}"
        );
        let message = resp
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            message.contains("Nonsense"),
            "missing key in message: {message}"
        );
    }

    #[test]
    fn handle_initialize_only_advertises_claude_channel_for_claude_clients() {
        let id = json!(1);
        let params = json!({ "protocolVersion": "2025-06-18" });

        let claude = handle_initialize(&id, &params, &connected_ctx_with(new_event_sink()));
        assert_eq!(
            claude.pointer("/result/capabilities/experimental/claude~1channel"),
            Some(&json!({}))
        );

        let codex = handle_initialize(
            &id,
            &params,
            &connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex),
        );
        assert!(
            codex
                .pointer("/result/capabilities/experimental/claude~1channel")
                .is_none(),
            "Codex must not advertise the Claude-specific channel capability: {codex}"
        );
        let instructions = codex
            .pointer("/result/instructions")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            instructions.contains("inject a one-shot nudge into the Codex pane"),
            "Codex instructions should explain pane-driven nudge delivery: {instructions}"
        );
        assert!(
            instructions.contains("actual peer request body comes from check_messages"),
            "Codex instructions should point Codex at check_messages for the real body: {instructions}"
        );
        assert!(instructions.contains("assemble every body page"));
        assert!(instructions.contains("explicitly ack only after the complete body was received"));
        assert!(instructions.contains("retry the same cursor without ack"));
        assert!(instructions.contains("confirms receipt, not completion"));
        assert!(instructions.contains("response contains only that confirmation"));
        assert!(instructions.contains("pending_after greater than zero"));
        assert!(instructions.contains("call check_messages({}) again immediately"));
        assert!(instructions.contains("renga may also send a follow-up nudge"));
        assert!(!instructions.contains("no nudge will follow"));
    }

    fn enqueue_test_message(ctx: &PeerCtx, body: String) {
        queue_pull_message(
            &ctx.inbox,
            QueuedPeerMessage {
                delivery_id: None,
                from_id: "2".to_string(),
                from_name: Some("planner".to_string()),
                from_kind: Some(PeerClientKind::Claude),
                body,
                sent_at: "2026-04-28T10:00:00Z".to_string(),
            },
        );
    }

    #[test]
    fn check_messages_spec_teaches_the_page_assemble_ack_loop() {
        let spec = tools_spec();
        let check = spec
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("check_messages"))
            .expect("check_messages tool");
        let description = check
            .get("description")
            .and_then(Value::as_str)
            .expect("description");
        assert!(description.contains("one bounded page"));
        assert!(description.contains("Assemble the complete body before acting"));
        assert!(description.contains("only that explicit ack removes the message"));
        assert!(description.contains("retry the same cursor without ack"));
        assert!(description.contains("ack response is confirmation only"));
        assert!(description.contains("pending_after is greater than zero"));
        assert!(description.contains("call check_messages({}) again immediately"));
        assert!(description.contains("renga may also send a follow-up nudge"));
        assert!(!description.contains("no nudge will follow"));
        assert_eq!(
            check.pointer("/inputSchema/properties/max_response_bytes/default"),
            Some(&json!(4096))
        );
        assert!(check
            .pointer("/inputSchema/properties/ack/description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("receipt, not completion"));
    }

    #[test]
    fn check_messages_debug_log_records_empty_call_once() {
        let path = debug_test_path("empty");
        let ctx = connected_ctx_with_debug_log(path.clone());

        let response = handle_check_messages(&json!(1), &json!({}), &ctx);

        assert_eq!(structured(&response).get("count"), Some(&json!(0)));
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get("action"), Some(&json!("check_messages")));
        assert_eq!(records[0].get("call_shape"), Some(&json!("empty")));
        assert_eq!(records[0].get("ack_result"), Some(&json!("none")));
        assert_eq!(records[0].pointer("/response/count"), Some(&json!(0)));
        assert_eq!(records[0].get("inbox_len_before"), Some(&json!(0)));
        assert_eq!(records[0].get("inbox_len_after"), Some(&json!(0)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn check_messages_debug_log_omits_complete_body_and_records_its_length() {
        let path = debug_test_path("complete-body");
        let ctx = connected_ctx_with_debug_log(path.clone());
        let body = "complete private body";
        enqueue_test_message(&ctx, body.to_string());

        handle_check_messages(&json!(1), &json!({}), &ctx);

        let text = std::fs::read_to_string(&path).expect("debug log");
        assert!(!text.contains(body));
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].pointer("/response/body_len"),
            Some(&json!(body.len()))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn check_messages_debug_log_records_cursor_call_once_without_body_or_token() {
        let path = debug_test_path("cursor");
        let ctx = connected_ctx_with_debug_log(path.clone());
        enqueue_test_message(&ctx, "message body that must stay private".repeat(300));
        let first = handle_check_messages_inner(&json!(1), &json!({}), &ctx).response;
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let next_offset = first
            .pointer("/result/structuredContent/delivery/next_offset_bytes")
            .and_then(Value::as_u64)
            .unwrap();

        handle_check_messages(
            &json!(2),
            &json!({"message_id": message_id, "offset_bytes": next_offset}),
            &ctx,
        );

        let text = std::fs::read_to_string(&path).expect("debug log");
        assert!(!text.contains("message body"));
        assert!(!text.contains("ack_token\""));
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get("call_shape"), Some(&json!("cursor")));
        assert_eq!(
            records[0].pointer("/args/message_id"),
            Some(&json!(message_id))
        );
        assert_eq!(
            records[0].pointer("/args/offset_bytes"),
            Some(&json!(next_offset))
        );
        assert!(
            records[0]
                .pointer("/response/body_len")
                .unwrap()
                .as_u64()
                .unwrap()
                > 0
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn check_messages_debug_log_records_ack_call_once_without_token() {
        let path = debug_test_path("ack");
        let ctx = connected_ctx_with_debug_log(path.clone());
        enqueue_test_message(&ctx, "ack me".to_string());
        let first = handle_check_messages_inner(&json!(1), &json!({}), &ctx).response;
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = first
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();

        handle_check_messages(
            &json!(2),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );

        let text = std::fs::read_to_string(&path).expect("debug log");
        assert!(!text.contains(token));
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get("call_shape"), Some(&json!("ack")));
        assert_eq!(records[0].get("ack_result"), Some(&json!("accepted")));
        assert_eq!(
            records[0].get("renudge_after_ack"),
            Some(&json!("skipped_none_pending"))
        );
        assert_eq!(
            records[0].pointer("/args/ack/message_id"),
            Some(&json!(message_id))
        );
        assert_eq!(
            records[0].pointer("/args/ack/token_present"),
            Some(&json!(true))
        );
        assert_eq!(records[0].get("inbox_len_before"), Some(&json!(1)));
        assert_eq!(records[0].get("inbox_len_after"), Some(&json!(0)));
        assert_eq!(records[0].pointer("/response/count"), Some(&json!(0)));
        assert_eq!(
            records[0].pointer("/response/head_message_id"),
            Some(&Value::Null),
            "an accepted ack records an empty response head, distinct from its accepted status"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn check_messages_ack_preserves_delivery_id_for_consumed_request() {
        let (ctx, requests) = connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        let sender = spawn_peer_inbox_ack_sender(ctx.clone());
        sender.enqueue(77);
        queue_pull_message(
            &ctx.inbox,
            QueuedPeerMessage {
                delivery_id: Some(77),
                from_id: "2".into(),
                from_name: Some("planner".into()),
                from_kind: Some(PeerClientKind::Claude),
                body: "ack me".into(),
                sent_at: "2026-09-06T12:40:00Z".into(),
            },
        );
        let first = handle_check_messages_inner(&json!(1), &json!({}), &ctx).response;
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = first
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();

        let handled = handle_check_messages_inner(
            &json!(2),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );

        assert_eq!(handled.consumed_after_ack, Some("sent"));
        sender.finish();
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[
                Request::PeerInboxAck {
                    pane_id: 1,
                    delivery_id: 77,
                },
                Request::PeerInboxConsumed {
                    pane_id: 1,
                    delivery_id: 77,
                },
            ]
        );
    }

    #[test]
    fn consumed_request_uses_async_queue_sync_fallback_or_reports_unavailable() {
        let (sent_ctx, sent_requests) =
            connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        let sender = spawn_peer_inbox_ack_sender(sent_ctx.clone());
        assert_eq!(request_peer_inbox_consumed(&sent_ctx, Some(11)), "sent");
        sender.finish();
        assert_eq!(sent_requests.lock().unwrap().len(), 1);

        let (sync_ctx, sync_requests) =
            connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        *sync_ctx.peer_inbox_request_sender.lock().unwrap() = PeerInboxRequestRoute::Sync;
        assert_eq!(
            request_peer_inbox_consumed(&sync_ctx, Some(12)),
            "sent_sync_fallback"
        );
        assert_eq!(sync_requests.lock().unwrap().len(), 1);

        let unavailable_ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        assert_eq!(
            request_peer_inbox_consumed(&unavailable_ctx, Some(13)),
            "rejected_sender_unavailable"
        );
    }

    #[test]
    fn failed_async_consumed_report_remains_for_reconciliation() {
        let (mut ctx, requests) =
            connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        ctx.request_sink_response = Some(Response::Err {
            message: "temporary failure".into(),
            code: Some(ipc::err_code::APP_TIMEOUT.into()),
        });
        retain_unreported_consumed(&ctx, 61);
        let sender = spawn_peer_inbox_ack_sender(ctx.clone());

        assert_eq!(request_peer_inbox_consumed(&ctx, Some(61)), "sent");
        sender.finish();

        assert_eq!(ctx.unreported_consumed.lock().unwrap().as_slices().0, &[61]);
        ctx.request_sink_response = Some(Response::ok_unit());
        assert!(reconcile_peer_inbox(&ctx));
        assert!(ctx.unreported_consumed.lock().unwrap().is_empty());
        assert!(matches!(
            requests.lock().unwrap().last(),
            Some(Request::PeerInboxReconcile {
                consumed,
                ..
            }) if consumed == &[61]
        ));
    }

    #[test]
    fn rejected_consumed_request_keeps_ack_response_unchanged() {
        let mut ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        ctx.request_sink = Some(Arc::new(Mutex::new(Vec::new())));
        ctx.request_sink_response = Some(Response::Err {
            message: "parse error: unknown variant peer_inbox_consumed".into(),
            code: Some(ipc::err_code::PARSE.into()),
        });
        *ctx.peer_inbox_request_sender.lock().unwrap() = PeerInboxRequestRoute::Sync;
        queue_pull_message(
            &ctx.inbox,
            QueuedPeerMessage {
                delivery_id: Some(88),
                from_id: "2".into(),
                from_name: None,
                from_kind: Some(PeerClientKind::Claude),
                body: "stable bytes".into(),
                sent_at: "2026-09-06T12:40:00Z".into(),
            },
        );
        let first = handle_check_messages_inner(&json!(1), &json!({}), &ctx).response;
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = first
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();
        let expected = acknowledged_check_messages_response(&json!(2), message_id, 0);

        let actual = handle_check_messages(
            &json!(2),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );

        assert_eq!(actual, expected);
    }

    #[test]
    fn reconcile_reports_held_and_unreported_then_clears_reported_ids() {
        let (ctx, requests) = connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        queue_pull_message(
            &ctx.inbox,
            QueuedPeerMessage {
                delivery_id: Some(41),
                from_id: "2".into(),
                from_name: None,
                from_kind: None,
                body: "held".into(),
                sent_at: "now".into(),
            },
        );
        retain_unreported_consumed(&ctx, 42);
        ctx.unreported_consumed_overflow.store(3, Ordering::Release);

        assert!(reconcile_peer_inbox(&ctx));

        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[Request::PeerInboxReconcile {
                pane_id: 1,
                held: vec![41],
                consumed: vec![42],
                held_overflow: 0,
                consumed_overflow: 3,
            }]
        );
        assert!(ctx.unreported_consumed.lock().unwrap().is_empty());
        assert_eq!(ctx.unreported_consumed_overflow.load(Ordering::Acquire), 0);
    }

    #[test]
    fn failed_reconcile_retains_unreported_consumed_for_next_connection() {
        let mut ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        ctx.request_sink = Some(Arc::new(Mutex::new(Vec::new())));
        ctx.request_sink_response = Some(Response::Err {
            message: "older server".into(),
            code: Some(ipc::err_code::PARSE.into()),
        });
        retain_unreported_consumed(&ctx, 52);
        ctx.unreported_consumed_overflow.store(2, Ordering::Release);

        assert!(!reconcile_peer_inbox(&ctx));
        assert_eq!(ctx.unreported_consumed.lock().unwrap().as_slices().0, &[52]);
        assert_eq!(ctx.unreported_consumed_overflow.load(Ordering::Acquire), 2);
    }

    #[test]
    fn successful_reconcile_preserves_concurrent_consumed_overflow() {
        let (mut ctx, requests) =
            connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        ctx.unreported_consumed_overflow.store(3, Ordering::Release);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        ctx.reconcile_snapshot_barrier = Some(barrier.clone());
        let reconcile_ctx = ctx.clone();
        let reconcile = thread::spawn(move || reconcile_peer_inbox(&reconcile_ctx));

        barrier.wait();
        ctx.unreported_consumed_overflow
            .fetch_add(1, Ordering::AcqRel);
        barrier.wait();

        assert!(reconcile.join().unwrap());
        assert_eq!(ctx.unreported_consumed_overflow.load(Ordering::Acquire), 1);
        assert!(matches!(
            requests.lock().unwrap().last(),
            Some(Request::PeerInboxReconcile {
                consumed_overflow: 3,
                ..
            })
        ));
    }

    #[test]
    fn push_peer_does_not_send_reconcile() {
        let (ctx, requests) = connected_ctx_with_requests(PeerClientKind::Claude, PUSH_READY_DELAY);
        assert!(reconcile_peer_inbox(&ctx));
        assert!(requests.lock().unwrap().is_empty());
    }

    #[test]
    fn reconcile_caps_held_ids_and_marks_overflow() {
        let (ctx, requests) = connected_ctx_with_requests(PeerClientKind::Codex, PUSH_READY_DELAY);
        for delivery_id in 1..=257 {
            queue_pull_message(
                &ctx.inbox,
                QueuedPeerMessage {
                    delivery_id: Some(delivery_id),
                    from_id: "2".into(),
                    from_name: None,
                    from_kind: None,
                    body: "held".into(),
                    sent_at: "now".into(),
                },
            );
        }
        assert!(reconcile_peer_inbox(&ctx));
        match &requests.lock().unwrap()[0] {
            Request::PeerInboxReconcile {
                held,
                held_overflow,
                ..
            } => {
                assert_eq!(held.len(), UNREPORTED_CONSUMED_CAP);
                assert_eq!(*held_overflow, 1);
            }
            other => panic!("expected reconcile request, got {other:?}"),
        };
    }

    #[test]
    fn rejected_renudge_request_keeps_ack_response_unchanged() {
        let path = debug_test_path("renudge-rejected");
        let mut ctx = connected_ctx_with_debug_log(path.clone());
        let request_sink = Arc::new(Mutex::new(Vec::new()));
        ctx.request_sink = Some(request_sink.clone());
        ctx.request_sink_response = Some(Response::Err {
            message: "parse error: unknown variant peer_inbox_head_acknowledged".to_string(),
            code: Some(ipc::err_code::PARSE.to_string()),
        });
        enqueue_test_message(&ctx, "first".to_string());
        enqueue_test_message(&ctx, "second".to_string());
        let first = handle_check_messages_inner(&json!(1), &json!({}), &ctx).response;
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = first
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();
        let expected = acknowledged_check_messages_response(&json!(2), message_id, 1);

        let actual = handle_check_messages(
            &json!(2),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );

        assert_eq!(actual, expected, "best-effort IPC must not alter ack bytes");
        assert_eq!(request_sink.lock().unwrap().len(), 1);
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].get("renudge_after_ack"),
            Some(&json!("rejected"))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unreachable_app_records_rejected_renudge_and_keeps_ack_response() {
        // No request_sink here, so the real client::send_request runs against
        // the dummy endpoint and fails at connect. That is the field shape of
        // a vanished socket or a wedged App, and it must be recorded as a
        // rejection rather than a delivery.
        let path = debug_test_path("renudge-unreachable");
        let ctx = connected_ctx_with_debug_log(path.clone());
        enqueue_test_message(&ctx, "first".to_string());
        enqueue_test_message(&ctx, "second".to_string());
        let first = handle_check_messages_inner(&json!(1), &json!({}), &ctx).response;
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = first
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();
        let expected = acknowledged_check_messages_response(&json!(2), message_id, 1);

        let actual = handle_check_messages(
            &json!(2),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );

        assert_eq!(
            actual, expected,
            "a failed re-nudge must not alter ack bytes"
        );
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].get("renudge_after_ack"),
            Some(&json!("rejected"))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn check_messages_debug_log_records_rejected_ack_and_null_response() {
        let path = debug_test_path("rejected-ack");
        let ctx = connected_ctx_with_debug_log(path.clone());
        enqueue_test_message(&ctx, "still queued".to_string());

        let response = handle_check_messages(
            &json!(1),
            &json!({"ack": {"message_id": "m1", "token": "wrong"}}),
            &ctx,
        );

        assert!(response.get("error").is_some());
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get("call_shape"), Some(&json!("ack")));
        assert_eq!(
            records[0].get("ack_result"),
            Some(&json!("rejected:ack does not match the queued FIFO head"))
        );
        assert_eq!(
            records[0].get("error_reason"),
            Some(&json!("ack does not match the queued FIFO head"))
        );
        assert_eq!(
            records[0].pointer("/response/head_message_id"),
            Some(&Value::Null)
        );
        assert_eq!(records[0].pointer("/response/count"), Some(&Value::Null));
        assert_eq!(records[0].get("inbox_len_before"), Some(&json!(1)));
        assert_eq!(records[0].get("inbox_len_after"), Some(&json!(1)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn disabled_check_messages_debug_log_writes_nothing_with_inherited_env() {
        let _env_guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = debug_test_path("disabled");
        let _env_restore = EnvVarRestore::set(ENV_DEBUG_CODEX_PEER_LOG, path.as_os_str());
        let ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);

        handle_check_messages(&json!(1), &json!({}), &ctx);
        log_peer_inbox_received(&ctx, Some(1), 2, 3, 4);
        log_peer_receipt_cache_hit(&ctx, Some(1));

        assert!(!path.exists());
    }

    #[test]
    fn peer_inbox_debug_records_contain_metadata_not_body() {
        let path = debug_test_path("inbox-events");
        let ctx = connected_ctx_with_debug_log(path.clone());

        log_peer_inbox_received(&ctx, Some(71), 9, "private payload".len(), 2);
        log_peer_receipt_cache_hit(&ctx, Some(71));
        log_peer_inbox_ack_sent(&path, 1, 71, true, None);
        log_peer_inbox_ack_sent(&path, 1, 72, false, Some("send failed"));

        let text = std::fs::read_to_string(&path).expect("debug log");
        assert!(!text.contains("private payload"));
        let records = read_debug_records(&path);
        assert_eq!(records.len(), 4);
        assert_eq!(
            records[0].get("action"),
            Some(&json!("peer_inbox_received"))
        );
        assert_eq!(records[0].get("delivery_id"), Some(&json!(71)));
        assert_eq!(records[0].get("from_pane"), Some(&json!(9)));
        assert_eq!(records[0].get("body_len"), Some(&json!(15)));
        assert_eq!(records[0].get("inbox_len_after"), Some(&json!(2)));
        assert_eq!(
            records[1].get("action"),
            Some(&json!("peer_receipt_cache_hit"))
        );
        assert_eq!(records[1].get("delivery_id"), Some(&json!(71)));
        assert_eq!(
            records[2].get("action"),
            Some(&json!("peer_inbox_ack_sent"))
        );
        assert_eq!(records[2].get("delivery_id"), Some(&json!(71)));
        assert_eq!(records[2].get("ok"), Some(&json!(true)));
        assert_eq!(records[2].get("error"), Some(&Value::Null));
        assert_eq!(
            records[3].get("action"),
            Some(&json!("peer_inbox_ack_sent"))
        );
        assert_eq!(records[3].get("delivery_id"), Some(&json!(72)));
        assert_eq!(records[3].get("ok"), Some(&json!(false)));
        assert_eq!(records[3].get("error"), Some(&json!("send failed")));
        for record in records {
            assert_eq!(record.get("pane_id"), Some(&json!(1)));
            assert!(record.get("process_id").is_some());
            assert!(record.get("record_sequence").is_some());
            assert!(record.get("timestamp_unix_ms").is_some());
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn peer_inbox_ack_queue_is_non_blocking_ordered_and_recorded() {
        let path = debug_test_path("ack-queue");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = connected_ctx_with_debug_log(path.clone());
        ctx.request_sink = Some(requests.clone());
        ctx.request_sink_response = Some(Response::ok_unit());

        // Holding the request sink makes the worker block inside the first
        // IPC attempt while the subscriber-facing queue remains available.
        let request_guard = requests.lock().unwrap();
        let sender = spawn_peer_inbox_ack_sender(ctx);
        sender.enqueue(71);
        let deadline = Instant::now() + Duration::from_secs(1);
        while sender.depth() != 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(sender.depth(), 0);
        sender.enqueue(72);
        assert_eq!(sender.depth(), 1);
        drop(request_guard);
        sender.finish();

        let delivery_ids = requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| match request {
                Request::PeerInboxAck { delivery_id, .. } => Some(*delivery_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(delivery_ids, vec![71, 72]);
        let records = read_debug_records(&path);
        assert_eq!(
            records
                .iter()
                .filter(|record| record.get("action") == Some(&json!("peer_inbox_ack_queued")))
                .count(),
            2
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.get("action") == Some(&json!("peer_inbox_ack_sent")))
                .count(),
            2
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn peer_inbox_subscription_path_consumes_next_event_while_ack_ipc_is_blocked() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        ctx.request_sink = Some(requests.clone());
        ctx.request_sink_response = Some(Response::ok_unit());
        let inbox = ctx.inbox.clone();
        let request_guard = requests.lock().unwrap();
        let ack_sender = spawn_peer_inbox_ack_sender(ctx.clone());
        let (processed_tx, processed_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut receipt_cache = PeerReceiptCache::default();
            for delivery_id in [71, 72] {
                assert!(handle_peer_subscription_event(
                    &ctx,
                    &ack_sender,
                    &mut receipt_cache,
                    &inbox,
                    PeerClientKind::Codex,
                    1,
                    ipc::Event::PeerInbox {
                        delivery_id: Some(delivery_id),
                        target_pane: 1,
                        from_pane: 9,
                        from_name: Some("sender".into()),
                        from_kind: Some(PeerClientKind::Claude),
                        body: format!("message-{delivery_id}"),
                        ts_ms: delivery_id,
                    },
                )
                .is_none());
            }
            let _ = processed_tx.send(());
            let _ = release_rx.recv();
            ack_sender.finish();
            inbox
        });

        let processed_while_ack_blocked = processed_rx
            .recv_timeout(Duration::from_millis(250))
            .is_ok();
        drop(request_guard);
        let _ = release_tx.send(());
        let inbox = worker.join().expect("subscription event worker");

        assert!(
            processed_while_ack_blocked,
            "subscription path blocked on acknowledgement IPC"
        );
        assert_eq!(
            inbox
                .lock()
                .unwrap_or_else(|messages| messages.into_inner())
                .messages
                .len(),
            2
        );
    }

    #[test]
    fn peer_inbox_ack_teardown_caps_drain_and_records_dropped_queue_depth() {
        let path = debug_test_path("ack-drain-cap");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = connected_ctx_with_debug_log(path.clone());
        ctx.request_sink = Some(requests.clone());
        ctx.request_sink_response = Some(Response::ok_unit());
        let request_guard = requests.lock().unwrap();
        let sender = spawn_peer_inbox_ack_sender(ctx);
        sender.enqueue(71);
        let deadline = Instant::now() + Duration::from_secs(1);
        while sender.depth() != 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        sender.enqueue(72);
        sender.enqueue(73);

        sender.finish_with_timeout(Duration::from_millis(20));

        let records = read_debug_records(&path);
        let abandoned = records
            .iter()
            .find(|record| record.get("action") == Some(&json!("peer_inbox_ack_drain_abandoned")))
            .expect("drain abandonment trace");
        assert_eq!(abandoned.get("pending_count"), Some(&json!(3)));
        assert_eq!(abandoned.get("dropped_count"), Some(&json!(2)));
        drop(request_guard);
        wait_for_request_count(&requests, 1);
        thread::sleep(Duration::from_millis(10));
        let _ = std::fs::remove_file(path);
    }

    fn collect_queued_body(ctx: &PeerCtx, args: Value) -> (String, String, String, usize) {
        let mut request_args = args.clone();
        let budget = parse_check_response_budget(&args).expect("response budget");
        let mut assembled = String::new();
        let mut pages = 0;
        loop {
            let response = handle_check_messages(&json!(pages + 1), &request_args, ctx);
            assert!(
                serialized_frame_len(&response) <= budget,
                "serialized page exceeded {budget} bytes: {}",
                serialized_frame_len(&response)
            );
            let delivery = response
                .pointer("/result/structuredContent/delivery")
                .expect("delivery");
            pages += 1;
            if let Some(body) = response
                .pointer("/result/structuredContent/messages/0/body")
                .and_then(Value::as_str)
            {
                assembled.push_str(body);
            } else {
                assembled.push_str(
                    delivery
                        .get("body_chunk")
                        .and_then(Value::as_str)
                        .expect("body chunk"),
                );
            }
            if delivery.get("complete").and_then(Value::as_bool) == Some(true) {
                return (
                    assembled,
                    delivery
                        .get("message_id")
                        .and_then(Value::as_str)
                        .expect("message id")
                        .to_string(),
                    delivery
                        .get("ack_token")
                        .and_then(Value::as_str)
                        .expect("ack token")
                        .to_string(),
                    pages,
                );
            }
            request_args = json!({
                "max_response_bytes": args
                    .get("max_response_bytes")
                    .cloned()
                    .unwrap_or(json!(CHECK_MESSAGES_DEFAULT_RESPONSE_BYTES)),
                "message_id": delivery.get("message_id").cloned().unwrap(),
                "offset_bytes": delivery.get("next_offset_bytes").cloned().unwrap(),
            });
        }
    }

    #[test]
    fn handle_check_messages_retains_short_message_until_explicit_ack() {
        let ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        enqueue_test_message(&ctx, "please inspect pane 4".to_string());

        let resp = handle_check_messages(&json!(1), &json!({}), &ctx);
        let body = structured(&resp);
        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array");
        assert_eq!(body.get("count").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].get("from_id").and_then(|v| v.as_str()),
            Some("2")
        );
        assert_eq!(
            messages[0].get("from_name").and_then(|v| v.as_str()),
            Some("planner")
        );
        assert_eq!(
            messages[0].get("from_kind").and_then(|v| v.as_str()),
            Some("claude")
        );
        assert_eq!(
            messages[0].get("body").and_then(|v| v.as_str()),
            Some("please inspect pane 4")
        );
        assert_eq!(body.get("pending_after").and_then(Value::as_u64), Some(0));
        assert_eq!(
            body.get("ack_required").and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            !resp
                .pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("please inspect pane 4"),
            "content must not duplicate the body"
        );

        let repeated = handle_check_messages(&json!(2), &json!({}), &ctx);
        assert_eq!(
            repeated.pointer("/result/structuredContent/messages/0/body"),
            resp.pointer("/result/structuredContent/messages/0/body")
        );
        let message_id = body
            .pointer("/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = body
            .pointer("/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();

        let drained = handle_check_messages(
            &json!(3),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );
        assert_eq!(
            structured(&drained).get("count").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert_eq!(
            drained.pointer("/result/structuredContent/messages"),
            Some(&json!([]))
        );
        assert!(
            drained
                .pointer("/result/structuredContent/delivery")
                .is_none(),
            "a successful ack must not include a delivery"
        );
        assert_eq!(
            drained.pointer("/result/structuredContent/acknowledged_message_id"),
            Some(&json!(message_id))
        );
        assert_eq!(
            drained.pointer("/result/structuredContent/ack_required"),
            Some(&json!(false))
        );
        // A final ack is confirmation, not the byte-identical empty-inbox response.
        assert_eq!(
            drained
                .pointer("/result/content/0/text")
                .and_then(|v| v.as_str()),
            Some(format!("Acknowledged {message_id}. No queued messages.").as_str())
        );
    }

    #[test]
    fn handle_check_messages_default_pages_realistic_japanese_message_losslessly() {
        let ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        let sentence = "将軍からの長文依頼を一文字も失わず、順番どおり安全に受け取ります。";
        let body = sentence.repeat(55);
        assert!((1500..=2200).contains(&body.chars().count()));
        enqueue_test_message(&ctx, body.clone());
        enqueue_test_message(&ctx, "next after pages".to_string());

        let (assembled, message_id, token, pages) = collect_queued_body(&ctx, json!({}));
        assert!(
            pages > 1,
            "the real-world-sized Japanese body must use multiple pages"
        );
        assert_eq!(assembled, body);
        let acknowledged = handle_check_messages(
            &json!(999),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );
        assert_eq!(
            structured(&acknowledged)
                .get("count")
                .and_then(Value::as_u64),
            Some(0)
        );
        assert_eq!(
            acknowledged.pointer("/result/structuredContent/pending_after"),
            Some(&json!(1))
        );
        assert!(
            acknowledged
                .pointer("/result/structuredContent/delivery")
                .is_none(),
            "ack after all pages must not include the next message"
        );
        let next = handle_check_messages(&json!(1000), &json!({}), &ctx);
        assert_eq!(
            next.pointer("/result/structuredContent/messages/0/body"),
            Some(&json!("next after pages"))
        );
    }

    #[test]
    fn handle_check_messages_large_payload_stays_bounded_and_retries_identically() {
        let ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        let unit = "日本語\n\t\\\"control\u{0008}";
        let body = unit.repeat((1024 * 1024 / unit.len()) + 1);
        assert!(body.len() > 1024 * 1024);
        enqueue_test_message(&ctx, body.clone());

        let first = handle_check_messages(&json!(1), &json!({}), &ctx);
        assert!(serialized_frame_len(&first) <= CHECK_MESSAGES_DEFAULT_RESPONSE_BYTES);
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let next_offset = first
            .pointer("/result/structuredContent/delivery/next_offset_bytes")
            .and_then(Value::as_u64)
            .unwrap();
        let retry = handle_check_messages(&json!(1), &json!({}), &ctx);
        assert_eq!(first, retry, "an unacknowledged cursor must be idempotent");
        assert_eq!(
            ctx.inbox.lock().unwrap().messages.len(),
            1,
            "rendering or dropping a response must not dequeue"
        );

        let second = handle_check_messages(
            &json!(2),
            &json!({"message_id": message_id, "offset_bytes": next_offset}),
            &ctx,
        );
        assert!(serialized_frame_len(&second) <= CHECK_MESSAGES_DEFAULT_RESPONSE_BYTES);
        let smaller = handle_check_messages(
            &json!(4),
            &json!({"max_response_bytes": 2048, "message_id": message_id, "offset_bytes": next_offset}),
            &ctx,
        );
        assert!(serialized_frame_len(&smaller) <= 2048);
        assert_eq!(
            second.pointer("/result/structuredContent/delivery/offset_bytes"),
            smaller.pointer("/result/structuredContent/delivery/offset_bytes")
        );
        let (assembled, message_id, token, pages) = collect_queued_body(&ctx, json!({}));
        assert!(pages > 100);
        assert_eq!(assembled, body);
        let done = handle_check_messages(
            &json!(3),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );
        assert_eq!(
            done.pointer("/result/structuredContent/has_more"),
            Some(&json!(false))
        );
        assert_eq!(
            done.pointer("/result/structuredContent/acknowledged_message_id"),
            Some(&json!(message_id))
        );
        assert!(done.pointer("/result/structuredContent/delivery").is_none());
    }

    #[test]
    fn unacknowledged_head_reports_messages_waiting_behind_it() {
        let ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        enqueue_test_message(&ctx, "first".to_string());
        let first = handle_check_messages(&json!(1), &json!({}), &ctx);
        enqueue_test_message(&ctx, "new arrival".to_string());

        let repeated = handle_check_messages(&json!(2), &json!({}), &ctx);
        assert_eq!(
            repeated.pointer("/result/structuredContent/messages/0/body"),
            Some(&json!("first"))
        );
        assert_eq!(
            repeated.pointer("/result/structuredContent/pending_after"),
            Some(&json!(1))
        );
        assert_eq!(
            repeated.pointer("/result/structuredContent/delivery/message_id"),
            first.pointer("/result/structuredContent/delivery/message_id")
        );
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = first
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();
        let acknowledged = handle_check_messages(
            &json!(3),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );
        // The ack response deliberately does not advance into the next body: callers process
        // that body as a fresh request after its own nudge and check_messages({}) call.
        assert_eq!(
            acknowledged.pointer("/result/structuredContent/messages"),
            Some(&json!([]))
        );
        assert_eq!(
            acknowledged.pointer("/result/structuredContent/pending_after"),
            Some(&json!(1))
        );
        assert_eq!(
            acknowledged.pointer("/result/structuredContent/has_more"),
            Some(&json!(true))
        );
        assert!(acknowledged
            .pointer("/result/structuredContent/delivery")
            .is_none());
        let advanced = handle_check_messages(&json!(4), &json!({}), &ctx);
        assert_eq!(
            advanced.pointer("/result/structuredContent/messages/0/body"),
            Some(&json!("new arrival"))
        );
        assert_eq!(
            advanced.pointer("/result/structuredContent/pending_after"),
            Some(&json!(0))
        );
        let duplicate_ack = handle_check_messages(
            &json!(5),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );
        assert_eq!(duplicate_ack.pointer("/error/code"), Some(&json!(-32602)));
        assert_eq!(ctx.inbox.lock().unwrap().messages.len(), 1);
    }

    #[test]
    fn coalesced_nudge_ack_rearms_app_without_a_fresh_check() {
        let mut app = App::new(40, 80).expect("App::new");
        let (_subscription_id, events) = app.event_bus.subscribe();
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
        while events.try_recv().is_ok() {}

        app.handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(codex_id),
            "first request".to_string(),
        )
        .expect("first send");
        app.handle_peer_send(
            sender_id,
            &ipc::PaneRef::Id(codex_id),
            "second request".to_string(),
        )
        .expect("second send");
        assert_eq!(
            app.pending_codex_peer_messages
                .get(&codex_id)
                .map(VecDeque::len),
            Some(1),
            "the second arrival shares the already-pending nudge"
        );

        let ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        for event in events.try_iter() {
            if let ipc::Event::PeerInbox {
                delivery_id,
                target_pane,
                from_pane,
                from_name,
                from_kind,
                body,
                ts_ms,
            } = event
            {
                assert_eq!(target_pane, codex_id);
                queue_pull_message(
                    &ctx.inbox,
                    QueuedPeerMessage {
                        delivery_id,
                        from_id: from_pane.to_string(),
                        from_name,
                        from_kind,
                        body,
                        sent_at: ts_ms_to_string(ts_ms),
                    },
                );
            }
        }
        assert_eq!(ctx.inbox.lock().unwrap().messages.len(), 2);
        let request_sink = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = ctx;
        ctx.mode = Mode::Connected {
            pane_id: codex_id,
            endpoint: dummy_endpoint(),
        };
        ctx.request_sink = Some(request_sink.clone());

        let first = handle_check_messages(&json!(1), &json!({}), &ctx);
        let message_id = first
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let token = first
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();
        let acknowledged = handle_check_messages(
            &json!(2),
            &json!({"ack": {"message_id": message_id, "token": token}}),
            &ctx,
        );
        assert_eq!(
            acknowledged.pointer("/result/structuredContent/pending_after"),
            Some(&json!(1))
        );
        assert_eq!(
            acknowledged.pointer("/result/structuredContent/has_more"),
            Some(&json!(true))
        );
        let acknowledgement_text = acknowledged
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(acknowledgement_text.contains("Call check_messages({}) again immediately"));
        assert!(acknowledgement_text.contains("renga may also send a follow-up nudge"));
        assert!(!acknowledgement_text.contains("no nudge will follow"));
        assert!(
            acknowledged
                .pointer("/result/structuredContent/delivery")
                .is_none(),
            "the acknowledgement must not chain the second body"
        );
        // The first nudge has done its job by the time Codex can acknowledge
        // the first body. No fresh check is made here: the ack's IPC request
        // alone must put a new Draft into the App queue.
        app.pending_codex_peer_messages.remove(&codex_id);
        let request = request_sink
            .lock()
            .unwrap()
            .pop()
            .expect("ack with one queued head requests a re-nudge");
        match request {
            Request::PeerInboxHeadAcknowledged {
                pane_id,
                remaining,
                next_from_pane,
                next_from_name,
                next_from_kind,
            } => app
                .handle_peer_inbox_head_acknowledged(
                    pane_id,
                    remaining,
                    next_from_pane,
                    next_from_name,
                    next_from_kind,
                )
                .expect("App accepts re-nudge request"),
            other => panic!("unexpected IPC request: {other:?}"),
        }
        assert!(app.pending_codex_peer_front_is_draft(codex_id));
        assert!(request_sink.lock().unwrap().is_empty());

        let second = handle_check_messages(&json!(3), &json!({}), &ctx);
        assert_eq!(
            second.pointer("/result/structuredContent/messages/0/body"),
            Some(&json!("second request"))
        );
        let second_message_id = second
            .pointer("/result/structuredContent/delivery/message_id")
            .and_then(Value::as_str)
            .unwrap();
        let second_token = second
            .pointer("/result/structuredContent/delivery/ack_token")
            .and_then(Value::as_str)
            .unwrap();
        let final_ack = handle_check_messages(
            &json!(4),
            &json!({"ack": {"message_id": second_message_id, "token": second_token}}),
            &ctx,
        );
        assert_eq!(
            final_ack.pointer("/result/structuredContent/pending_after"),
            Some(&json!(0))
        );
        assert!(
            request_sink.lock().unwrap().is_empty(),
            "ack with nothing queued must not request another nudge"
        );
        app.shutdown();
    }

    #[test]
    fn stale_ack_and_cursor_do_not_mutate_inbox() {
        let ctx = connected_ctx_with_kind(new_event_sink(), PeerClientKind::Codex);
        enqueue_test_message(&ctx, "keep me".to_string());
        for args in [
            json!({"ack": {"message_id": "m1", "token": "m1:ack"}}),
            json!({"ack": {"message_id": "wrong", "token": "wrong"}}),
            json!({"message_id": "wrong", "offset_bytes": 0}),
            json!({"offset_bytes": 1}),
        ] {
            let response = handle_check_messages(&json!(1), &args, &ctx);
            assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
            assert_eq!(ctx.inbox.lock().unwrap().messages.len(), 1);
        }
        let retained = handle_check_messages(&json!(2), &json!({}), &ctx);
        assert_eq!(
            retained.pointer("/result/structuredContent/messages/0/body"),
            Some(&json!("keep me")),
            "rejected acknowledgements and cursors must leave the FIFO head readable"
        );
    }

    #[test]
    fn handle_send_keys_detached_surfaces_friendly_text() {
        let ctx = detached_ctx("RENGA_PANE_ID not set");
        let id = json!(1);
        let resp = handle_send_keys(
            &id,
            &json!({ "target": "1", "text": "y", "enter": true }),
            &ctx,
        );
        let text = resp
            .pointer("/result/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            text.contains("renga not reachable"),
            "expected friendly detached text, got {text:?}"
        );
    }

    #[test]
    fn send_keys_schema_requires_target() {
        let spec = tools_spec();
        let entry = spec
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("send_keys"))
            .expect("send_keys entry");
        let required: Vec<&str> = entry
            .get("inputSchema")
            .and_then(|s| s.get("required"))
            .and_then(|r| r.as_array())
            .expect("required array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["target"]);
    }

    #[test]
    fn scan_buffer_reports_window_max_even_when_filter_excludes_all() {
        let mut buf = EventBuffer::default();
        buf.push(pane_started_value(1, 10));
        buf.push(pane_started_value(2, 20));
        let filter = vec!["pane_exited".to_string()];
        let scan = scan_buffer(&buf, 1, Some(&filter));
        assert!(scan.matched.is_empty());
        assert_eq!(scan.window_max_seq, Some(2));
    }

    #[test]
    fn scan_buffer_skips_events_before_cursor() {
        let mut buf = EventBuffer::default();
        buf.push(pane_started_value(1, 10));
        buf.push(pane_exited_value(2, 20));
        let scan = scan_buffer(&buf, 2, None);
        assert_eq!(scan.window_max_seq, Some(2));
        assert_eq!(scan.matched.len(), 1);
        assert_eq!(scan.matched[0].get("id").and_then(|v| v.as_u64()), Some(2));
    }

    #[test]
    fn event_matches_filter_accepts_when_filter_absent_or_empty() {
        let ev = pane_exited_value(1, 0);
        assert!(event_matches_filter(&ev, None));
        let empty: Vec<String> = Vec::new();
        assert!(event_matches_filter(&ev, Some(&empty)));
    }

    #[test]
    fn event_matches_filter_checks_type_field() {
        let ev = pane_exited_value(1, 0);
        let yes = vec!["pane_exited".to_string(), "pane_started".to_string()];
        let no = vec!["pane_started".to_string()];
        assert!(event_matches_filter(&ev, Some(&yes)));
        assert!(!event_matches_filter(&ev, Some(&no)));
    }

    #[test]
    fn should_buffer_for_poll_excludes_heartbeat_and_peer_inbox() {
        assert!(!should_buffer_for_poll(&ipc::Event::Heartbeat { ts_ms: 1 }));
        assert!(!should_buffer_for_poll(&ipc::Event::PeerInbox {
            delivery_id: None,
            target_pane: 1,
            from_pane: 2,
            from_name: None,
            from_kind: None,
            body: "x".into(),
            ts_ms: 1,
        }));
        assert!(should_buffer_for_poll(&ipc::Event::PaneStarted {
            id: 1,
            name: None,
            role: None,
            ts_ms: 1,
        }));
        assert!(should_buffer_for_poll(&ipc::Event::PaneExited {
            id: 1,
            name: None,
            role: None,
            ts_ms: 1,
        }));
        assert!(should_buffer_for_poll(&ipc::Event::EventsDropped {
            count: 3,
            ts_ms: 1,
        }));
    }

    #[test]
    fn peer_receipt_cache_makes_reemitted_delivery_id_idempotent() {
        let mut cache = PeerReceiptCache::default();
        let mut retained = Vec::new();
        assert!(retain_peer_delivery_once(&mut cache, Some(41), || {
            retained.push("body");
            true
        }));
        assert!(retain_peer_delivery_once(&mut cache, Some(41), || {
            retained.push("body");
            true
        }));
        assert_eq!(retained, vec!["body"]);
        assert_eq!(cache.ids.iter().copied().collect::<Vec<_>>(), vec![41]);
    }

    #[test]
    fn effective_poll_timeout_applies_default_and_clamp() {
        // Pure-function test so we can exercise the clamp without
        // actually blocking a test thread for POLL_MAX_TIMEOUT_MS.
        assert_eq!(
            effective_poll_timeout(None),
            Duration::from_millis(POLL_DEFAULT_TIMEOUT_MS)
        );
        assert_eq!(effective_poll_timeout(Some(0)), Duration::from_millis(0));
        assert_eq!(
            effective_poll_timeout(Some(500)),
            Duration::from_millis(500)
        );
        assert_eq!(
            effective_poll_timeout(Some(10_000_000)),
            Duration::from_millis(POLL_MAX_TIMEOUT_MS)
        );
        assert_eq!(
            effective_poll_timeout(Some(u64::MAX)),
            Duration::from_millis(POLL_MAX_TIMEOUT_MS)
        );
        // Compile-time guard: a future change that silently bumps
        // POLL_MAX_TIMEOUT_MS past 60 s should not compile at all.
        const _: () = assert!(POLL_MAX_TIMEOUT_MS <= 60_000);
    }

    #[test]
    fn handle_poll_events_detached_returns_empty_without_blocking() {
        let ctx = detached_ctx("no socket");
        let start = Instant::now();
        let resp = handle_poll_events(&json!(1), &json!({ "timeout_ms": 5_000 }), &ctx);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "detached mode must not block; elapsed = {:?}",
            start.elapsed()
        );
        let body = structured(&resp);
        assert_eq!(body.get("next_since").and_then(|v| v.as_str()), Some("0"));
        assert!(body
            .get("events")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty()));
    }

    #[test]
    fn handle_poll_events_since_absent_starts_from_now_and_times_out_empty() {
        let events = new_event_sink();
        {
            let (lock, _) = &*events;
            let mut buf = lock.lock().unwrap();
            buf.push(pane_started_value(1, 10));
            buf.push(pane_exited_value(1, 20));
        }
        let ctx = connected_ctx_with(events);
        let resp = handle_poll_events(&json!(1), &json!({ "timeout_ms": 0 }), &ctx);
        let body = structured(&resp);
        assert!(body
            .get("events")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty()));
        assert_eq!(body.get("next_since").and_then(|v| v.as_str()), Some("2"));
    }

    #[test]
    fn handle_poll_events_with_since_returns_strictly_after_cursor() {
        let events = new_event_sink();
        {
            let (lock, _) = &*events;
            let mut buf = lock.lock().unwrap();
            buf.push(pane_started_value(1, 10));
            buf.push(pane_exited_value(1, 20));
            buf.push(pane_started_value(2, 30));
        }
        let ctx = connected_ctx_with(events);
        let resp = handle_poll_events(&json!(1), &json!({ "since": "1", "timeout_ms": 0 }), &ctx);
        let body = structured(&resp);
        let arr = body.get("events").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr.len(), 2, "expected seqs 2 and 3, got {arr:?}");
        assert_eq!(body.get("next_since").and_then(|v| v.as_str()), Some("3"));
    }

    #[test]
    fn handle_poll_events_types_filter_narrows_matched_but_advances_cursor() {
        let events = new_event_sink();
        {
            let (lock, _) = &*events;
            let mut buf = lock.lock().unwrap();
            buf.push(pane_started_value(1, 10));
            buf.push(pane_exited_value(1, 20));
            buf.push(pane_started_value(2, 30));
        }
        let ctx = connected_ctx_with(events);
        let resp = handle_poll_events(
            &json!(1),
            &json!({ "since": "0", "timeout_ms": 0, "types": ["pane_exited"] }),
            &ctx,
        );
        let body = structured(&resp);
        let arr = body.get("events").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0].get("type").and_then(|v| v.as_str()),
            Some("pane_exited")
        );
        assert_eq!(body.get("next_since").and_then(|v| v.as_str()), Some("3"));
    }

    #[test]
    fn handle_poll_events_timeout_zero_returns_immediately() {
        let ctx = connected_ctx_with(new_event_sink());
        let start = Instant::now();
        let resp = handle_poll_events(&json!(1), &json!({ "timeout_ms": 0 }), &ctx);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "zero timeout must be non-blocking; elapsed = {:?}",
            start.elapsed()
        );
        let body = structured(&resp);
        assert_eq!(body.get("next_since").and_then(|v| v.as_str()), Some("0"));
    }

    #[test]
    fn handle_poll_events_wakes_on_notify_before_deadline() {
        let events = new_event_sink();
        let ctx = connected_ctx_with(events.clone());
        let handle = thread::spawn(move || {
            handle_poll_events(&json!(1), &json!({ "timeout_ms": 10_000 }), &ctx)
        });
        thread::sleep(Duration::from_millis(50));
        {
            let (lock, cvar) = &*events;
            let mut buf = lock.lock().unwrap();
            buf.push(pane_exited_value(7, 42));
            cvar.notify_all();
        }
        let start = Instant::now();
        let resp = handle.join().expect("poll worker panicked");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "notify failed to wake the poll; elapsed = {:?}",
            start.elapsed()
        );
        let body = structured(&resp);
        let arr = body.get("events").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("id").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(body.get("next_since").and_then(|v| v.as_str()), Some("1"));
    }

    #[test]
    fn tools_call_routes_to_pane_control_handlers() {
        // Smoke test on the dispatch: each new tool name must route
        // through handle_tools_call rather than falling through to
        // the unknown-tool arm. In detached mode, list_panes /
        // spawn_pane / close_pane / focus_pane / new_tab either emit
        // the friendly "renga not reachable" text (result.isError =
        // false) or the -32602 we already test for; none of them
        // should ever surface a -32601 "unknown tool" here.
        let ctx = detached_ctx("not relevant");
        let id = json!(1);
        for (name, args) in [
            ("list_panes", json!({})),
            ("spawn_pane", json!({ "direction": "vertical" })),
            ("spawn_codex_pane", json!({ "direction": "vertical" })),
            ("close_pane", json!({ "target": "1" })),
            ("focus_pane", json!({ "target": "1" })),
            ("new_tab", json!({})),
            ("inspect_pane", json!({ "target": "1" })),
            ("send_keys", json!({ "target": "1", "text": "y" })),
            ("poll_events", json!({ "timeout_ms": 0 })),
        ] {
            let params = json!({ "name": name, "arguments": args });
            let resp = handle_tools_call(&id, &params, &ctx).expect("dispatch");
            let err_code = resp
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64());
            assert_ne!(
                err_code,
                Some(-32601),
                "{name} fell through to unknown-tool arm: {resp}"
            );
        }
    }

    #[test]
    fn channel_notification_body_starts_with_peer_banner() {
        // renga#221 acceptance criterion #1: a peer notification must
        // be visually distinguishable from a real user turn even
        // when Claude Code renders it under a `Human:` heading. The
        // body wrap inside `peer_banner_wrap` is what carries that
        // signal — make sure it actually reaches the channel push.
        let note = channel_notification("hi there", "7", Some("dispatcher"));
        let content = note
            .pointer("/params/content")
            .and_then(|v| v.as_str())
            .expect("content string");
        assert!(
            content.starts_with("📡 PEER MESSAGE"),
            "channel content must start with the peer-message banner; got {content:?}"
        );
        assert!(
            content.contains("dispatcher"),
            "banner should name the sender so an operator can tell who spoke; got {content:?}"
        );
        assert!(
            content.contains("(id=7)"),
            "banner should include the from_id; got {content:?}"
        );
        assert!(
            content.contains("NOT FROM USER"),
            "banner must explicitly disclaim user-input semantics; got {content:?}"
        );
        assert!(
            content.ends_with("hi there"),
            "original body must be preserved verbatim after the banner; got {content:?}"
        );
    }

    #[test]
    fn channel_notification_banner_handles_missing_from_name() {
        // EventsDropped synthesizes its own from_name, but anonymous
        // senders (no display name) still need a clean banner.
        let note = channel_notification("payload", "12", None);
        let content = note
            .pointer("/params/content")
            .and_then(|v| v.as_str())
            .expect("content string");
        assert!(
            content.starts_with("📡 PEER MESSAGE — from id=12 — NOT FROM USER"),
            "missing from_name should fall back to id-only header; got {content:?}"
        );
    }
}
