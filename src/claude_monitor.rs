//! Claude Code session monitoring via JSONL transcript files.
//!
//! Watches ~/.claude/projects/<project>/*.jsonl for real-time events:
//! tool uses, sub-agent spawns (isSidechain), thinking state.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// A single todo item from TodoWrite tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: String, // "pending", "in_progress", "completed"
}

/// Current state of a Claude session inferred from JSONL events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeState {
    /// Last tool used (Bash, Read, Edit, Task, etc.)
    pub current_tool: Option<String>,
    /// Active sub-agent count (isSidechain sessions currently running)
    pub subagent_count: usize,
    /// Names of active sub-agent types (e.g. "evaluator", "generator")
    pub subagent_types: Vec<String>,
    /// True if Claude is currently thinking/processing
    pub is_working: bool,
    /// Total tool uses in this session
    pub tool_use_count: usize,
    /// Current model (e.g. "claude-opus-4-6")
    pub model: Option<String>,
    /// Cumulative token usage
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Current todo list (from TodoWrite)
    pub todos: Vec<TodoItem>,
    /// Current context window size (last message's total input tokens)
    pub context_tokens: u64,
    /// Git branch of the last assistant message
    pub git_branch: Option<String>,
}

impl ClaudeState {
    /// Total tokens used.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    /// Cache hit rate (0.0..1.0).
    #[allow(dead_code)]
    pub fn cache_hit_rate(&self) -> f64 {
        let total_input = self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens;
        if total_input == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / total_input as f64
        }
    }

    /// Todo completion stats: (completed, total).
    pub fn todo_progress(&self) -> (usize, usize) {
        let completed = self
            .todos
            .iter()
            .filter(|t| t.status == "completed")
            .count();
        (completed, self.todos.len())
    }

    /// Context window limit for the current model (in tokens).
    ///
    /// Claude Code writes the plain model id (e.g. `claude-opus-4-6`)
    /// into the JSONL, **without** the `[1m]` suffix even when the
    /// session is running the 1M variant. Opus 4.6 ships with a 1M
    /// context by default for Pro / Max users, so it's treated as 1M
    /// here. Older Opus uses a 500K baseline so the status-bar usage
    /// ratio better reflects the practical working window on those
    /// models. Haiku and Sonnet keep their native 200K window — the
    /// `[1m]` extended variants are still picked up by the explicit
    /// `[1m]` / `-1m` suffix path above. Unknown models fall back to
    /// 200K as the safe default.
    pub fn context_limit(&self) -> u64 {
        match self.model.as_deref() {
            Some(m) if m.contains("[1m]") || m.contains("-1m") => 1_000_000,
            // Opus 4.6+: 1M context is default.
            Some(m) if m.contains("opus-4-6") => 1_000_000,
            Some(m) if m.contains("haiku") => 200_000,
            Some(m) if m.contains("sonnet") => 200_000,
            Some(m) if m.contains("opus") => 500_000,
            _ => 200_000,
        }
    }

    /// Context usage ratio (0.0..1.0).
    pub fn context_usage(&self) -> f64 {
        let limit = self.context_limit();
        if limit == 0 {
            0.0
        } else {
            (self.context_tokens as f64 / limit as f64).min(1.0)
        }
    }

    /// Short model name for display (e.g. "opus-4-6" → "opus").
    pub fn short_model(&self) -> Option<&str> {
        let full = self.model.as_deref()?;
        if full.contains("opus") {
            Some("opus")
        } else if full.contains("sonnet") {
            Some("sonnet")
        } else if full.contains("haiku") {
            Some("haiku")
        } else {
            Some(full)
        }
    }
}

/// Cumulative state for one transcript file.
struct TranscriptMonitor {
    file_position: u64,
    last_mtime: Option<SystemTime>,
    initializing: bool,
    state: ClaudeState,
    /// Active sub-agents: tool_use_id → subagent_type (or "general-purpose")
    active_task_ids: BTreeMap<String, String>,
    /// Request IDs already counted for token usage (avoid double-counting).
    counted_request_ids: std::collections::HashSet<String>,
}

impl TranscriptMonitor {
    fn new() -> Self {
        Self {
            file_position: 0,
            last_mtime: None,
            initializing: true,
            state: ClaudeState::default(),
            active_task_ids: BTreeMap::new(),
            counted_request_ids: std::collections::HashSet::new(),
        }
    }
}

struct PaneView {
    last_check: Instant,
    state: ClaudeState,
}

impl PaneView {
    fn new() -> Self {
        Self {
            last_check: Instant::now() - Duration::from_secs(10),
            state: ClaudeState::default(),
        }
    }
}

#[derive(Default)]
struct SharedState {
    panes: HashMap<usize, PaneView>,
}

#[derive(Default)]
struct WorkerRequests {
    pending: HashMap<usize, PathBuf>,
    removals: HashSet<usize>,
}

/// Shared state and the dedicated transcript monitor worker.
#[derive(Clone)]
pub struct ClaudeMonitor {
    core: Arc<MonitorCore>,
}

struct MonitorCore {
    shared: Arc<Mutex<SharedState>>,
    requests: Arc<Mutex<WorkerRequests>>,
    wake_tx: Mutex<Option<SyncSender<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    worker_alive: Arc<AtomicBool>,
    metrics: Arc<WorkerMetrics>,
    trace_path: Mutex<Option<OsString>>,
    filesystem: Arc<dyn MonitorFilesystem>,
}

#[derive(Default)]
struct WorkerMetrics {
    bytes_read: AtomicUsize,
    lines_parsed: AtomicUsize,
    path_changes: AtomicUsize,
    mtime_changes: AtomicUsize,
    dropped_wakes: AtomicU64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClaudeMonitorActivity {
    pub bytes_read: usize,
    pub lines_parsed: usize,
    pub path_changes: usize,
    pub mtime_changes: usize,
}

#[derive(Clone, Copy)]
struct TranscriptMetadata {
    len: u64,
    modified: Option<SystemTime>,
}

trait MonitorFilesystem: Send + Sync {
    fn path_exists(&self, path: &Path) -> bool;
    fn find_jsonl_path(&self, cwd: &Path) -> Option<PathBuf>;
    fn metadata(&self, path: &Path) -> Option<TranscriptMetadata>;
    fn read_batch(
        &self,
        path: &Path,
        read_from: u64,
        max_bytes: usize,
    ) -> Option<(Vec<String>, u64, usize)>;
}

struct RealMonitorFilesystem;

impl MonitorFilesystem for RealMonitorFilesystem {
    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn find_jsonl_path(&self, cwd: &Path) -> Option<PathBuf> {
        find_jsonl_path(cwd)
    }

    fn metadata(&self, path: &Path) -> Option<TranscriptMetadata> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(TranscriptMetadata {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    fn read_batch(
        &self,
        path: &Path,
        read_from: u64,
        max_bytes: usize,
    ) -> Option<(Vec<String>, u64, usize)> {
        read_transcript_batch(path, read_from, max_bytes)
    }
}

/// Throttle interval for file metadata checks (to avoid per-frame syscalls).
const CHECK_INTERVAL: Duration = Duration::from_millis(500);
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);
const WORKER_YIELD: Duration = Duration::from_millis(10);
const MAX_BYTES_PER_TICK: usize = 4 * 1024 * 1024;
const MAX_TRANSCRIPTS_PER_PROJECT: usize = 8;
const MAX_TRANSCRIPTS_TOTAL: usize = 64;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum cached request IDs for token dedup. JSONL is read sequentially
/// and we never re-read old lines, so clearing the set is safe — the only
/// cost is a potential double-count of the very last request if it spans
/// two read batches (negligible).
const MAX_REQUEST_ID_CACHE: usize = 10_000;

impl ClaudeMonitor {
    pub fn new() -> Self {
        Self::new_with_filesystem(Arc::new(RealMonitorFilesystem))
    }

    fn new_with_filesystem(filesystem: Arc<dyn MonitorFilesystem>) -> Self {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let requests = Arc::new(Mutex::new(WorkerRequests::default()));
        Self {
            core: Arc::new(MonitorCore {
                shared,
                requests,
                wake_tx: Mutex::new(None),
                worker: Mutex::new(None),
                stop: Arc::new(AtomicBool::new(false)),
                worker_alive: Arc::new(AtomicBool::new(false)),
                metrics: Arc::new(WorkerMetrics::default()),
                trace_path: Mutex::new(None),
                filesystem,
            }),
        }
    }

    /// Start the worker explicitly after App construction.
    pub fn start(&self) {
        self.start_with_trace(crate::app::codex_peer_debug_log_path());
    }

    fn start_with_trace(&self, trace_path: Option<OsString>) {
        let mut worker_slot = match self.core.worker.lock() {
            Ok(worker) => worker,
            Err(_) => return,
        };
        if worker_slot.is_some() {
            return;
        }
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        if let Ok(mut tx) = self.core.wake_tx.lock() {
            *tx = Some(wake_tx);
        } else {
            return;
        }
        if let Ok(mut path) = self.core.trace_path.lock() {
            *path = trace_path.clone();
        }
        self.core.stop.store(false, Ordering::Release);
        self.core.worker_alive.store(true, Ordering::Release);
        let shared = Arc::clone(&self.core.shared);
        let requests = Arc::clone(&self.core.requests);
        let metrics = Arc::clone(&self.core.metrics);
        let stop = Arc::clone(&self.core.stop);
        let worker_alive = Arc::clone(&self.core.worker_alive);
        let filesystem = Arc::clone(&self.core.filesystem);
        let panic_trace_path = trace_path.clone();
        let worker = thread::Builder::new()
            .name("claude-monitor".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    MonitorWorker::new(
                        shared.clone(),
                        requests.clone(),
                        metrics,
                        stop,
                        trace_path,
                        filesystem,
                    )
                    .run(wake_rx);
                }));
                if result.is_err() {
                    worker_alive.store(false, Ordering::Release);
                    if let Ok(mut state) = shared.lock() {
                        state.panes.clear();
                    }
                    if let Ok(mut pending) = requests.lock() {
                        pending.pending.clear();
                    }
                    write_worker_stopped_trace(panic_trace_path.as_deref(), "panic");
                }
            });
        match worker {
            Ok(worker) => {
                *worker_slot = Some(worker);
            }
            Err(_) => {
                self.core.worker_alive.store(false, Ordering::Release);
                if let Ok(mut tx) = self.core.wake_tx.lock() {
                    tx.take();
                }
            }
        }
    }

    /// Get the current state for a pane.
    pub fn state(&self, pane_id: usize) -> ClaudeState {
        if !self.core.worker_alive.load(Ordering::Acquire) {
            return ClaudeState::default();
        }
        self.core
            .shared
            .lock()
            .ok()
            .and_then(|state| state.panes.get(&pane_id).map(|pane| pane.state.clone()))
            .unwrap_or_default()
    }

    /// Update monitoring for a pane with its current cwd.
    /// Throttled to CHECK_INTERVAL; this calling thread performs no filesystem work.
    pub fn update(&self, pane_id: usize, cwd: &Path) {
        if !self.core.worker_alive.load(Ordering::Acquire) {
            return;
        }
        {
            let mut state = match self.core.shared.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            let pane = state.panes.entry(pane_id).or_insert_with(PaneView::new);
            if pane.last_check.elapsed() < CHECK_INTERVAL {
                return;
            }
            pane.last_check = Instant::now();
        }
        if let Ok(mut requests) = self.core.requests.lock() {
            requests.pending.insert(pane_id, cwd.to_path_buf());
        } else {
            return;
        }
        self.wake_worker();
    }

    pub fn remove(&self, pane_id: usize) {
        if let Ok(mut state) = self.core.shared.lock() {
            state.panes.remove(&pane_id);
        }
        if let Ok(mut requests) = self.core.requests.lock() {
            requests.pending.remove(&pane_id);
            requests.removals.insert(pane_id);
        }
        self.wake_worker();
    }

    fn wake_worker(&self) {
        let tx = match self.core.wake_tx.lock() {
            Ok(tx) => tx,
            Err(_) => return,
        };
        let Some(tx) = tx.as_ref() else {
            return;
        };
        match tx.try_send(()) {
            Ok(()) => {}
            Err(TrySendError::Full(())) => {
                self.core
                    .metrics
                    .dropped_wakes
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(())) => {
                self.core.worker_alive.store(false, Ordering::Release);
                if let Ok(mut state) = self.core.shared.lock() {
                    state.panes.clear();
                }
            }
        }
    }

    pub(crate) fn take_worker_activity(&self) -> ClaudeMonitorActivity {
        ClaudeMonitorActivity {
            bytes_read: self.core.metrics.bytes_read.swap(0, Ordering::AcqRel),
            lines_parsed: self.core.metrics.lines_parsed.swap(0, Ordering::AcqRel),
            path_changes: self.core.metrics.path_changes.swap(0, Ordering::AcqRel),
            mtime_changes: self.core.metrics.mtime_changes.swap(0, Ordering::AcqRel),
        }
    }

    #[cfg(test)]
    pub(crate) fn record_worker_activity_for_test(&self, activity: ClaudeMonitorActivity) {
        self.core
            .metrics
            .bytes_read
            .fetch_add(activity.bytes_read, Ordering::Relaxed);
        self.core
            .metrics
            .lines_parsed
            .fetch_add(activity.lines_parsed, Ordering::Relaxed);
        self.core
            .metrics
            .path_changes
            .fetch_add(activity.path_changes, Ordering::Relaxed);
        self.core
            .metrics
            .mtime_changes
            .fetch_add(activity.mtime_changes, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn queue_update_for_test(&self, pane_id: usize, cwd: &Path) {
        if let Ok(mut state) = self.core.shared.lock() {
            state.panes.entry(pane_id).or_insert_with(PaneView::new);
        }
        if let Ok(mut requests) = self.core.requests.lock() {
            requests.pending.insert(pane_id, cwd.to_path_buf());
        }
        self.wake_worker();
    }

    /// Stop the worker without letting monitor cleanup delay TUI shutdown indefinitely.
    pub fn shutdown(&self) {
        self.core.stop.store(true, Ordering::Release);
        if let Ok(mut tx) = self.core.wake_tx.lock() {
            if let Some(sender) = tx.take() {
                let _ = sender.try_send(());
            }
        }
        let worker = self
            .core
            .worker
            .lock()
            .ok()
            .and_then(|mut worker| worker.take());
        let Some(worker) = worker else {
            return;
        };
        let started_at = Instant::now();
        while !worker.is_finished() && started_at.elapsed() < SHUTDOWN_TIMEOUT {
            thread::sleep(Duration::from_millis(10));
        }
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            let trace_path = self
                .core
                .trace_path
                .lock()
                .ok()
                .and_then(|path| path.clone());
            write_worker_stopped_trace(trace_path.as_deref(), "shutdown_timeout");
        }
        self.core.worker_alive.store(false, Ordering::Release);
        if let Ok(mut state) = self.core.shared.lock() {
            state.panes.clear();
        }
        if let Ok(mut requests) = self.core.requests.lock() {
            requests.pending.clear();
        }
    }
}

impl Default for ClaudeMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MonitorCore {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(tx) = self.wake_tx.get_mut() {
            tx.take();
        }
    }
}

struct WorkerPane {
    cwd: PathBuf,
    jsonl_path: Option<PathBuf>,
    last_rescan: Instant,
}

impl WorkerPane {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            jsonl_path: None,
            last_rescan: Instant::now() - Duration::from_secs(60),
        }
    }
}

#[derive(Default)]
struct ProjectCache {
    transcripts: HashMap<PathBuf, CachedTranscript>,
}

struct CachedTranscript {
    monitor: TranscriptMonitor,
    last_used: u64,
}

struct MonitorWorker {
    shared: Arc<Mutex<SharedState>>,
    requests: Arc<Mutex<WorkerRequests>>,
    metrics: Arc<WorkerMetrics>,
    stop: Arc<AtomicBool>,
    trace_path: Option<OsString>,
    panes: HashMap<usize, WorkerPane>,
    projects: HashMap<PathBuf, ProjectCache>,
    work_queue: VecDeque<usize>,
    queued: HashSet<usize>,
    generation: u64,
    filesystem: Arc<dyn MonitorFilesystem>,
}

impl MonitorWorker {
    fn new(
        shared: Arc<Mutex<SharedState>>,
        requests: Arc<Mutex<WorkerRequests>>,
        metrics: Arc<WorkerMetrics>,
        stop: Arc<AtomicBool>,
        trace_path: Option<OsString>,
        filesystem: Arc<dyn MonitorFilesystem>,
    ) -> Self {
        Self {
            shared,
            requests,
            metrics,
            stop,
            trace_path,
            panes: HashMap::new(),
            projects: HashMap::new(),
            work_queue: VecDeque::new(),
            queued: HashSet::new(),
            generation: 0,
            filesystem,
        }
    }

    fn run(mut self, wake_rx: Receiver<()>) {
        while !self.stop.load(Ordering::Acquire) {
            let wake = if self.work_queue.is_empty() {
                wake_rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                wake_rx.recv_timeout(WORKER_YIELD)
            };
            match wake {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            self.trace_dropped_wakes();
            self.drain_requests();
            if let Some(pane_id) = self.work_queue.pop_front() {
                self.queued.remove(&pane_id);
                if self.process_pane(pane_id) && !self.stop.load(Ordering::Acquire) {
                    self.enqueue(pane_id);
                }
            }
        }
    }

    fn trace_dropped_wakes(&self) {
        let count = self.metrics.dropped_wakes.swap(0, Ordering::AcqRel);
        if count == 0 {
            return;
        }
        write_worker_record(
            self.trace_path.as_deref(),
            serde_json::json!({
                "action": "claude_monitor_request_dropped",
                "count": count,
            }),
        );
    }

    fn drain_requests(&mut self) {
        let (pending, removals) = match self.requests.lock() {
            Ok(mut requests) => (
                std::mem::take(&mut requests.pending),
                std::mem::take(&mut requests.removals),
            ),
            Err(_) => return,
        };
        for pane_id in removals {
            self.panes.remove(&pane_id);
            self.queued.remove(&pane_id);
            self.work_queue.retain(|queued| *queued != pane_id);
        }
        for (pane_id, cwd) in pending {
            if self.panes.get(&pane_id).is_none_or(|pane| pane.cwd != cwd) {
                self.panes.insert(pane_id, WorkerPane::new(cwd));
            }
            self.enqueue(pane_id);
        }
        self.evict_caches();
    }

    fn enqueue(&mut self, pane_id: usize) {
        if self.panes.contains_key(&pane_id) && self.queued.insert(pane_id) {
            self.work_queue.push_back(pane_id);
        }
    }

    fn process_pane(&mut self, pane_id: usize) -> bool {
        let started_at = Instant::now();
        let Some(mut pane) = self.panes.remove(&pane_id) else {
            return false;
        };
        let path_missing = pane
            .jsonl_path
            .as_ref()
            .is_none_or(|path| !self.filesystem.path_exists(path));
        let should_rescan = path_missing || pane.last_rescan.elapsed() > RESCAN_INTERVAL;
        let mut path_changed = false;
        let mut resumed_from_cache = false;
        if should_rescan {
            pane.last_rescan = Instant::now();
            let selected = self.filesystem.find_jsonl_path(&pane.cwd);
            path_changed = pane.jsonl_path != selected;
            if path_changed {
                self.metrics.path_changes.fetch_add(1, Ordering::Relaxed);
                pane.jsonl_path = selected;
                let state = if let Some(path) = pane.jsonl_path.as_ref() {
                    let project = transcript_project_key(path);
                    let (state, existed) = self.cached_state(&project, path);
                    resumed_from_cache = existed;
                    if existed {
                        state
                    } else {
                        ClaudeState::default()
                    }
                } else {
                    ClaudeState::default()
                };
                publish_state(&self.shared, pane_id, state);
            }
        }

        let Some(path) = pane.jsonl_path.clone() else {
            self.panes.insert(pane_id, pane);
            if path_changed {
                self.trace_work(pane_id, None, 0, 0, false, started_at.elapsed());
            }
            return false;
        };
        let project = transcript_project_key(&path);

        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cached = self
            .projects
            .entry(project)
            .or_default()
            .transcripts
            .entry(path.clone())
            .or_insert_with(|| CachedTranscript {
                monitor: TranscriptMonitor::new(),
                last_used: generation,
            });
        cached.last_used = generation;
        let monitor = &mut cached.monitor;
        let meta = match self.filesystem.metadata(&path) {
            Some(meta) => meta,
            None => {
                self.panes.insert(pane_id, pane);
                return false;
            }
        };
        let mtime = meta.modified;
        if meta.len < monitor.file_position {
            *monitor = TranscriptMonitor::new();
            publish_state(&self.shared, pane_id, ClaudeState::default());
        }
        if mtime != monitor.last_mtime {
            self.metrics.mtime_changes.fetch_add(1, Ordering::Relaxed);
            monitor.last_mtime = mtime;
        } else if meta.len <= monitor.file_position {
            if !monitor.initializing {
                publish_state(&self.shared, pane_id, monitor.state.clone());
            }
            self.panes.insert(pane_id, pane);
            if path_changed {
                self.trace_work(
                    pane_id,
                    Some(&path),
                    0,
                    0,
                    resumed_from_cache,
                    started_at.elapsed(),
                );
            }
            return false;
        }

        let Some((lines, new_position, bytes_read)) =
            self.filesystem
                .read_batch(&path, monitor.file_position, MAX_BYTES_PER_TICK)
        else {
            self.panes.insert(pane_id, pane);
            return false;
        };
        monitor.file_position = new_position;
        for line in &lines {
            process_event(monitor, line);
        }
        self.metrics
            .bytes_read
            .fetch_add(bytes_read, Ordering::Relaxed);
        self.metrics
            .lines_parsed
            .fetch_add(lines.len(), Ordering::Relaxed);
        let more_to_read = monitor.file_position < meta.len && !lines.is_empty();
        if !more_to_read {
            monitor.initializing = false;
        }
        if !monitor.initializing {
            publish_state(&self.shared, pane_id, monitor.state.clone());
        }
        self.panes.insert(pane_id, pane);
        self.trace_work(
            pane_id,
            Some(&path),
            bytes_read,
            lines.len(),
            resumed_from_cache,
            started_at.elapsed(),
        );
        self.evict_caches();
        more_to_read
    }

    fn cached_state(&mut self, project: &Path, path: &Path) -> (ClaudeState, bool) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cache = self.projects.entry(project.to_path_buf()).or_default();
        if let Some(cached) = cache.transcripts.get_mut(path) {
            cached.last_used = generation;
            return (cached.monitor.state.clone(), !cached.monitor.initializing);
        }
        cache.transcripts.insert(
            path.to_path_buf(),
            CachedTranscript {
                monitor: TranscriptMonitor::new(),
                last_used: generation,
            },
        );
        (ClaudeState::default(), false)
    }

    fn evict_caches(&mut self) {
        let protected: HashSet<PathBuf> = self
            .panes
            .values()
            .filter_map(|pane| pane.jsonl_path.clone())
            .collect();
        for cache in self.projects.values_mut() {
            while cache.transcripts.len() > MAX_TRANSCRIPTS_PER_PROJECT {
                let Some(oldest) = oldest_unprotected(&cache.transcripts, &protected) else {
                    break;
                };
                cache.transcripts.remove(&oldest);
            }
        }
        while self
            .projects
            .values()
            .map(|cache| cache.transcripts.len())
            .sum::<usize>()
            > MAX_TRANSCRIPTS_TOTAL
        {
            let candidate = self
                .projects
                .iter()
                .flat_map(|(cwd, cache)| {
                    cache
                        .transcripts
                        .iter()
                        .filter(|(path, _)| !protected.contains(*path))
                        .map(|(path, cached)| (cwd.clone(), path.clone(), cached.last_used))
                })
                .min_by_key(|(_, _, used)| *used);
            let Some((cwd, path, _)) = candidate else {
                break;
            };
            if let Some(cache) = self.projects.get_mut(&cwd) {
                cache.transcripts.remove(&path);
            }
        }
        self.projects
            .retain(|_, cache| !cache.transcripts.is_empty());
    }

    fn trace_work(
        &self,
        pane_id: usize,
        path: Option<&Path>,
        bytes_read: usize,
        lines_parsed: usize,
        resumed_from_cache: bool,
        elapsed: Duration,
    ) {
        write_worker_record(
            self.trace_path.as_deref(),
            serde_json::json!({
                "action": "claude_monitor_worker",
                "pane_id": pane_id,
                "path": path.map(|path| path.to_string_lossy()),
                "bytes_read": bytes_read,
                "lines_parsed": lines_parsed,
                "resumed_from_cache": resumed_from_cache,
                "elapsed_ms": elapsed.as_millis(),
            }),
        );
    }
}

fn publish_state(shared: &Mutex<SharedState>, pane_id: usize, state: ClaudeState) {
    if let Ok(mut shared) = shared.lock() {
        if let Some(pane) = shared.panes.get_mut(&pane_id) {
            pane.state = state;
        }
    }
}

fn oldest_unprotected(
    transcripts: &HashMap<PathBuf, CachedTranscript>,
    protected: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    transcripts
        .iter()
        .filter(|(path, _)| !protected.contains(*path))
        .min_by_key(|(_, cached)| cached.last_used)
        .map(|(path, _)| path.clone())
}

fn transcript_project_key(path: &Path) -> PathBuf {
    path.parent().unwrap_or(path).to_path_buf()
}

fn read_transcript_batch(
    path: &Path,
    read_from: u64,
    max_bytes: usize,
) -> Option<(Vec<String>, u64, usize)> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(read_from)).ok()?;

    let mut new_lines = Vec::new();
    let mut new_position = read_from;
    let mut bytes_read = 0usize;
    let mut buf = String::new();
    loop {
        buf.clear();
        let bytes = match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        bytes_read += bytes;
        if !buf.ends_with('\n') {
            break;
        }
        new_position += bytes as u64;
        new_lines.push(buf.clone());
        if bytes_read >= max_bytes {
            break;
        }
    }
    Some((new_lines, new_position, bytes_read))
}

fn write_worker_record(path: Option<&std::ffi::OsStr>, record: serde_json::Value) {
    if let Some(path) = path {
        crate::app::append_codex_peer_debug_record(path, record);
    }
}

fn write_worker_stopped_trace(path: Option<&std::ffi::OsStr>, reason: &str) {
    write_worker_record(
        path,
        serde_json::json!({
            "action": "claude_monitor_worker_stopped",
            "reason": reason,
        }),
    );
}

/// Process a single JSONL line and update the monitor state.
fn process_event(monitor: &mut TranscriptMonitor, line: &str) {
    let json: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return,
    };

    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "assistant" => {
            let message = json.get("message");

            let stop_reason = message
                .and_then(|m| m.get("stop_reason"))
                .and_then(|v| v.as_str());

            // Any non-tool_use stop_reason means Claude finished this turn.
            // tool_use or null means still working.
            match stop_reason {
                Some("tool_use") | None => {
                    monitor.state.is_working = true;
                }
                Some(_) => {
                    monitor.state.is_working = false;
                    monitor.state.current_tool = None;
                }
            }

            // Model name
            if let Some(model) = message
                .and_then(|m| m.get("model"))
                .and_then(|v| v.as_str())
            {
                monitor.state.model = Some(model.to_string());
            }

            // Token usage — count once per requestId (avoid double-counting
            // when the same request is split across multiple JSONL lines)
            let request_id = json
                .get("requestId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // Only count tokens when requestId is present (to dedupe).
            // Missing requestId means we can't safely deduplicate, so skip counting.
            let should_count = match &request_id {
                Some(id) => {
                    if monitor.counted_request_ids.len() >= MAX_REQUEST_ID_CACHE {
                        monitor.counted_request_ids.clear();
                    }
                    monitor.counted_request_ids.insert(id.clone())
                }
                None => false,
            };

            if should_count {
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_create = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    monitor.state.input_tokens += input;
                    monitor.state.output_tokens += output;
                    monitor.state.cache_read_tokens += cache_read;
                    monitor.state.cache_creation_tokens += cache_create;

                    // Current context = input + cache (this is how much is sent each turn)
                    monitor.state.context_tokens = input + cache_read + cache_create;
                }
            }

            // Git branch (stored on every event; update if present)
            if let Some(branch) = json.get("gitBranch").and_then(|v| v.as_str()) {
                if !branch.is_empty() && branch != "HEAD" {
                    monitor.state.git_branch = Some(branch.to_string());
                }
            }

            let content = message
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());

            if let Some(content) = content {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "tool_use" {
                        if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                            monitor.state.current_tool = Some(name.to_string());
                            monitor.state.tool_use_count += 1;
                            monitor.state.is_working = true;

                            // Sub-agent tools (real name in JSONL is "Agent", "Task" was old name)
                            if name == "Agent" || name == "Task" {
                                if let Some(task_id) = block.get("id").and_then(|v| v.as_str()) {
                                    let subagent_type = block
                                        .get("input")
                                        .and_then(|i| i.get("subagent_type"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("general-purpose")
                                        .to_string();
                                    monitor
                                        .active_task_ids
                                        .insert(task_id.to_string(), subagent_type);
                                    monitor.state.subagent_count = monitor.active_task_ids.len();
                                    monitor.state.subagent_types =
                                        monitor.active_task_ids.values().cloned().collect();
                                }
                            }

                            // TodoWrite — parse the todos
                            if name == "TodoWrite" {
                                if let Some(todos_arr) = block
                                    .get("input")
                                    .and_then(|v| v.get("todos"))
                                    .and_then(|v| v.as_array())
                                {
                                    monitor.state.todos = todos_arr
                                        .iter()
                                        .filter_map(|t| {
                                            Some(TodoItem {
                                                content: t.get("content")?.as_str()?.to_string(),
                                                status: t.get("status")?.as_str()?.to_string(),
                                            })
                                        })
                                        .collect();
                                }
                            }
                        }
                    }
                }
            }
        }
        "user" => {
            // User message indicates either a new prompt OR a tool_result
            let content = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());

            let mut has_tool_result = false;
            if let Some(content) = content {
                for block in content {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        has_tool_result = true;
                        // If this tool_result is for a Task, decrement the active set
                        if let Some(tool_use_id) = block.get("tool_use_id").and_then(|v| v.as_str())
                        {
                            if monitor.active_task_ids.remove(tool_use_id).is_some() {
                                monitor.state.subagent_count = monitor.active_task_ids.len();
                                monitor.state.subagent_types =
                                    monitor.active_task_ids.values().cloned().collect();
                            }
                        }
                    }
                }
            }

            if !has_tool_result {
                // New user prompt — reset working state
                monitor.state.is_working = false;
                monitor.state.current_tool = None;
            }
        }
        _ => {}
    }
}

/// Convert a cwd path to Claude's project directory name and find the most recent JSONL.
fn find_jsonl_path(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let projects_dir = home.join(".claude").join("projects");

    if !projects_dir.exists() {
        return None;
    }

    let encoded = encode_cwd_to_project_name(cwd);
    let project_dir = projects_dir.join(&encoded);

    if !project_dir.exists() {
        return None;
    }

    let mut latest: Option<(PathBuf, SystemTime)> = None;
    let entries = std::fs::read_dir(&project_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    match &latest {
                        Some((_, old_mtime)) if *old_mtime >= mtime => {}
                        _ => latest = Some((path, mtime)),
                    }
                }
            }
        }
    }
    latest.map(|(p, _)| p)
}

/// Encode a path to Claude's project name format.
/// Claude Code replaces any character that is not ASCII alphanumeric or `.` with `-`.
/// E.g.,  `C:\Users\じゅぶ\dev` → `C--Users-----dev`
fn encode_cwd_to_project_name(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' {
            result.push(ch);
        } else {
            result.push('-');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn temp_transcript(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "renga-claude-monitor-{label}-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ))
    }

    fn apply_to_end(path: &Path, monitor: &mut TranscriptMonitor, max_bytes: usize) {
        let len = std::fs::metadata(path).expect("transcript metadata").len();
        if len < monitor.file_position {
            *monitor = TranscriptMonitor::new();
        }
        loop {
            let (lines, new_position, _) =
                read_transcript_batch(path, monitor.file_position, max_bytes)
                    .expect("read transcript");
            monitor.file_position = new_position;
            for line in &lines {
                process_event(monitor, line);
            }
            if lines.is_empty() || monitor.file_position >= len {
                monitor.initializing = false;
                break;
            }
        }
    }

    #[test]
    fn read_transcript_batch_reports_bytes_read_and_complete_lines_for_this_call() {
        let path = temp_transcript("read");
        let first = "{\"type\":\"first\"}\n";
        let second = "{\"type\":\"second\"}\n";
        let incomplete = "{\"type\":\"partial\"}";
        std::fs::write(&path, format!("{first}{second}{incomplete}"))
            .expect("write transcript fixture");

        let (lines, new_position, bytes_read) =
            read_transcript_batch(&path, first.len() as u64, usize::MAX)
                .expect("read transcript batch");
        assert_eq!(lines, vec![second]);
        assert_eq!(new_position, (first.len() + second.len()) as u64);
        assert_eq!(bytes_read, second.len() + incomplete.len());

        std::fs::remove_file(path).expect("remove transcript fixture");
    }

    #[test]
    fn transcript_cache_matches_one_pass_across_flip_append_and_truncation() {
        use std::io::Write;

        let path_a = temp_transcript("flip-a");
        let path_b = temp_transcript("flip-b");
        let a_initial = concat!(
            "{\"type\":\"assistant\",\"requestId\":\"req-a\",\"gitBranch\":\"feature\",\"message\":{\"model\":\"claude-opus-4-6\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Agent\",\"id\":\"z-task\",\"input\":{\"subagent_type\":\"reviewer\"}},{\"type\":\"tool_use\",\"name\":\"Agent\",\"id\":\"a-task\",\"input\":{\"subagent_type\":\"builder\"}}],\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"cache_read_input_tokens\":20},\"stop_reason\":\"tool_use\"}}\n",
            "{\"type\":\"assistant\",\"requestId\":\"req-a\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n"
        );
        let b = "{\"type\":\"assistant\",\"requestId\":\"req-b\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":3},\"stop_reason\":\"end_turn\"}}\n";
        let a_append = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"z-task\"}]}}\n",
            "{\"type\":\"assistant\",\"requestId\":\"req-c\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"TodoWrite\",\"id\":\"todo\",\"input\":{\"todos\":[{\"content\":\"ship\",\"status\":\"in_progress\"}]}}],\"usage\":{\"input_tokens\":4,\"output_tokens\":1},\"stop_reason\":\"tool_use\"}}\n"
        );
        std::fs::write(&path_a, a_initial).expect("write A");
        std::fs::write(&path_b, b).expect("write B");

        let mut cache = HashMap::<PathBuf, TranscriptMonitor>::new();
        apply_to_end(
            &path_a,
            cache
                .entry(path_a.clone())
                .or_insert_with(TranscriptMonitor::new),
            80,
        );
        assert_eq!(
            cache[&path_a].state.subagent_types,
            vec!["builder", "reviewer"]
        );
        apply_to_end(
            &path_b,
            cache
                .entry(path_b.clone())
                .or_insert_with(TranscriptMonitor::new),
            80,
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path_a)
            .expect("open A append")
            .write_all(a_append.as_bytes())
            .expect("append A");
        apply_to_end(&path_a, cache.get_mut(&path_a).expect("cached A"), 80);

        let mut golden = TranscriptMonitor::new();
        apply_to_end(&path_a, &mut golden, usize::MAX);
        let cached = cache.get(&path_a).expect("cached A after flip");
        assert_eq!(cached.state, golden.state);
        assert_eq!(cached.active_task_ids, golden.active_task_ids);
        assert_eq!(cached.counted_request_ids, golden.counted_request_ids);
        assert_eq!(cached.file_position, golden.file_position);
        assert_eq!(cached.state.subagent_types, vec!["builder"]);

        let replacement = "{\"type\":\"assistant\",\"requestId\":\"replacement\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"content\":[],\"usage\":{\"input_tokens\":3},\"stop_reason\":\"end_turn\"}}\n";
        std::fs::write(&path_a, replacement).expect("truncate A");
        apply_to_end(&path_a, cache.get_mut(&path_a).expect("cached A"), 80);
        let mut truncated_golden = TranscriptMonitor::new();
        apply_to_end(&path_a, &mut truncated_golden, usize::MAX);
        assert_eq!(cache[&path_a].state, truncated_golden.state);
        assert_eq!(
            cache[&path_a].counted_request_ids,
            truncated_golden.counted_request_ids
        );

        std::fs::remove_file(path_a).expect("remove A");
        std::fs::remove_file(path_b).expect("remove B");
    }

    #[test]
    fn request_id_cache_clear_matches_one_pass_reference() {
        use std::io::Write;

        let path = temp_transcript("request-cache");
        let mut file = File::create(&path).expect("create request transcript");
        for index in 0..MAX_REQUEST_ID_CACHE {
            writeln!(file, "{{\"type\":\"assistant\",\"requestId\":\"req-{index}\",\"message\":{{\"content\":[],\"usage\":{{\"input_tokens\":1}}}}}}")
                .expect("write request");
        }
        drop(file);
        let mut cached = TranscriptMonitor::new();
        apply_to_end(&path, &mut cached, 32 * 1024);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append requests");
        writeln!(file, "{{\"type\":\"assistant\",\"requestId\":\"req-new\",\"message\":{{\"content\":[],\"usage\":{{\"input_tokens\":1}}}}}}")
            .expect("write clearing request");
        writeln!(file, "{{\"type\":\"assistant\",\"requestId\":\"req-0\",\"message\":{{\"content\":[],\"usage\":{{\"input_tokens\":1}}}}}}")
            .expect("write repeated request");
        drop(file);
        apply_to_end(&path, &mut cached, 32 * 1024);
        let mut golden = TranscriptMonitor::new();
        apply_to_end(&path, &mut golden, usize::MAX);
        assert_eq!(cached.state, golden.state);
        assert_eq!(cached.counted_request_ids, golden.counted_request_ids);
        std::fs::remove_file(path).expect("remove request transcript");
    }

    struct SlowRecordingFilesystem {
        accesses: Arc<Mutex<Vec<thread::ThreadId>>>,
        entered: Option<mpsc::Sender<()>>,
        release: Option<Arc<AtomicBool>>,
    }

    impl MonitorFilesystem for SlowRecordingFilesystem {
        fn path_exists(&self, _path: &Path) -> bool {
            self.accesses
                .lock()
                .expect("access log")
                .push(thread::current().id());
            false
        }

        fn find_jsonl_path(&self, _cwd: &Path) -> Option<PathBuf> {
            self.accesses
                .lock()
                .expect("access log")
                .push(thread::current().id());
            if let Some(entered) = &self.entered {
                let _ = entered.send(());
            }
            if let Some(release) = &self.release {
                while !release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            } else {
                thread::sleep(Duration::from_millis(200));
            }
            None
        }

        fn metadata(&self, _path: &Path) -> Option<TranscriptMetadata> {
            None
        }

        fn read_batch(
            &self,
            _path: &Path,
            _read_from: u64,
            _max_bytes: usize,
        ) -> Option<(Vec<String>, u64, usize)> {
            None
        }
    }

    #[test]
    fn update_and_state_stay_responsive_while_worker_is_in_filesystem() {
        let accesses = Arc::new(Mutex::new(Vec::new()));
        let monitor = ClaudeMonitor::new_with_filesystem(Arc::new(SlowRecordingFilesystem {
            accesses: Arc::clone(&accesses),
            entered: None,
            release: None,
        }));
        monitor.start_with_trace(None);
        let caller = thread::current().id();
        let started = Instant::now();
        monitor.update(1, Path::new("slow-project"));
        assert!(started.elapsed() < Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_secs(1);
        while accesses.lock().expect("access log").is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let state_started = Instant::now();
        assert_eq!(monitor.state(1), ClaudeState::default());
        assert!(state_started.elapsed() < Duration::from_millis(50));
        let second_update_started = Instant::now();
        monitor.update(2, Path::new("another-project"));
        assert!(second_update_started.elapsed() < Duration::from_millis(50));
        let observed = accesses.lock().expect("access log").clone();
        assert!(!observed.is_empty());
        assert!(observed.into_iter().all(|thread_id| thread_id != caller));
        monitor.shutdown();
    }

    #[test]
    fn full_wake_channel_is_nonblocking_and_traced() {
        let path = temp_transcript("full-channel-trace");
        let accesses = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new(AtomicBool::new(false));
        let monitor = ClaudeMonitor::new_with_filesystem(Arc::new(SlowRecordingFilesystem {
            accesses,
            entered: Some(entered_tx),
            release: Some(Arc::clone(&release)),
        }));
        monitor.start_with_trace(Some(path.as_os_str().to_owned()));
        monitor.queue_update_for_test(1, Path::new("blocked"));
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered filesystem");
        monitor.queue_update_for_test(2, Path::new("queued"));
        let started = Instant::now();
        monitor.queue_update_for_test(3, Path::new("coalesced"));
        assert!(started.elapsed() < Duration::from_millis(50));
        release.store(true, Ordering::Release);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut contents = String::new();
        while Instant::now() < deadline {
            contents = std::fs::read_to_string(&path).unwrap_or_default();
            if contents.contains("claude_monitor_request_dropped") {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(contents.contains("claude_monitor_request_dropped"));
        monitor.shutdown();
        std::fs::remove_file(path).expect("remove channel trace");
    }

    struct BlockingReadFilesystem {
        path: PathBuf,
        entered: mpsc::Sender<()>,
        release: Arc<AtomicBool>,
    }

    impl MonitorFilesystem for BlockingReadFilesystem {
        fn path_exists(&self, path: &Path) -> bool {
            path.exists()
        }

        fn find_jsonl_path(&self, _cwd: &Path) -> Option<PathBuf> {
            Some(self.path.clone())
        }

        fn metadata(&self, path: &Path) -> Option<TranscriptMetadata> {
            RealMonitorFilesystem.metadata(path)
        }

        fn read_batch(
            &self,
            path: &Path,
            read_from: u64,
            max_bytes: usize,
        ) -> Option<(Vec<String>, u64, usize)> {
            let _ = self.entered.send(());
            while !self.release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            read_transcript_batch(path, read_from, max_bytes)
        }
    }

    #[test]
    fn remove_prevents_a_late_worker_result_from_recreating_the_pane() {
        let path = temp_transcript("late-result");
        std::fs::write(
            &path,
            "{\"type\":\"assistant\",\"requestId\":\"late\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n",
        )
        .expect("write transcript");
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new(AtomicBool::new(false));
        let monitor = ClaudeMonitor::new_with_filesystem(Arc::new(BlockingReadFilesystem {
            path: path.clone(),
            entered: entered_tx,
            release: Arc::clone(&release),
        }));
        monitor.start_with_trace(None);
        monitor.update(41, Path::new("project"));
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered read");
        monitor.remove(41);
        release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        while monitor.core.metrics.lines_parsed.load(Ordering::Acquire) == 0
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(10));
        assert!(!monitor
            .core
            .shared
            .lock()
            .expect("published state")
            .panes
            .contains_key(&41));
        monitor.shutdown();
        std::fs::remove_file(path).expect("remove transcript");
    }

    struct SwitchingFilesystem {
        selected: Arc<Mutex<Option<PathBuf>>>,
    }

    impl MonitorFilesystem for SwitchingFilesystem {
        fn path_exists(&self, path: &Path) -> bool {
            path.exists()
        }

        fn find_jsonl_path(&self, _cwd: &Path) -> Option<PathBuf> {
            self.selected.lock().expect("selected path").clone()
        }

        fn metadata(&self, path: &Path) -> Option<TranscriptMetadata> {
            RealMonitorFilesystem.metadata(path)
        }

        fn read_batch(
            &self,
            path: &Path,
            read_from: u64,
            max_bytes: usize,
        ) -> Option<(Vec<String>, u64, usize)> {
            read_transcript_batch(path, read_from, max_bytes)
        }
    }

    struct TraceEnvRestore(Option<OsString>);

    impl TraceEnvRestore {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("RENGA_DEBUG_CODEX_PEER_LOG");
            std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", path);
            Self(previous)
        }
    }

    impl Drop for TraceEnvRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", value),
                None => std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG"),
            }
            crate::app::set_codex_peer_debug_log_path_test_override(Some(None));
        }
    }

    #[test]
    fn start_captures_the_test_overridden_trace_path_before_spawning() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let trap_path = temp_transcript("start-env-trap");
        let enabled_path = temp_transcript("start-enabled-trace");
        let transcript = temp_transcript("start-fixture");
        std::fs::write(
            &transcript,
            "{\"type\":\"assistant\",\"requestId\":\"start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n",
        )
        .expect("write transcript");
        let _env = TraceEnvRestore::set(&trap_path);

        crate::app::set_codex_peer_debug_log_path_test_override(Some(None));
        let disabled = ClaudeMonitor::new_with_filesystem(Arc::new(SwitchingFilesystem {
            selected: Arc::new(Mutex::new(Some(transcript.clone()))),
        }));
        disabled.start();
        disabled.update(1, Path::new("project"));
        let deadline = Instant::now() + Duration::from_secs(1);
        while disabled.core.metrics.lines_parsed.load(Ordering::Acquire) == 0
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        disabled.shutdown();
        assert!(!trap_path.exists());

        crate::app::set_codex_peer_debug_log_path_test_override(Some(Some(
            enabled_path.as_os_str().to_owned(),
        )));
        let enabled = ClaudeMonitor::new_with_filesystem(Arc::new(SwitchingFilesystem {
            selected: Arc::new(Mutex::new(Some(transcript.clone()))),
        }));
        enabled.start();
        enabled.update(2, Path::new("project"));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !enabled_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        enabled.shutdown();
        let contents = std::fs::read_to_string(&enabled_path).expect("worker trace");
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("\"action\":\"claude_monitor_worker\""));
        assert!(!trap_path.exists());

        std::fs::remove_file(enabled_path).expect("remove enabled trace");
        std::fs::remove_file(transcript).expect("remove transcript");
    }

    fn force_rescan(worker: &mut MonitorWorker, pane_id: usize) {
        worker
            .panes
            .get_mut(&pane_id)
            .expect("worker pane")
            .last_rescan = Instant::now() - RESCAN_INTERVAL - Duration::from_millis(1);
    }

    #[test]
    fn worker_keeps_unknown_state_hidden_and_resumes_known_path_at_cached_position() {
        use std::io::Write;

        let path_a = temp_transcript("worker-a");
        let path_b = temp_transcript("worker-b");
        let line_a = "{\"type\":\"assistant\",\"requestId\":\"a\",\"message\":{\"model\":\"claude-opus-4-6\",\"content\":[],\"usage\":{\"input_tokens\":1}}}\n";
        let repeat = MAX_BYTES_PER_TICK / line_a.len() + 10;
        std::fs::write(&path_a, line_a.repeat(repeat)).expect("write large A");
        std::fs::write(
            &path_b,
            "{\"type\":\"assistant\",\"requestId\":\"b\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"content\":[],\"usage\":{\"input_tokens\":2}}}\n",
        )
        .expect("write B");

        let selected = Arc::new(Mutex::new(Some(path_a.clone())));
        let shared = Arc::new(Mutex::new(SharedState::default()));
        shared
            .lock()
            .expect("shared")
            .panes
            .insert(1, PaneView::new());
        let metrics = Arc::new(WorkerMetrics::default());
        let mut worker = MonitorWorker::new(
            Arc::clone(&shared),
            Arc::new(Mutex::new(WorkerRequests::default())),
            Arc::clone(&metrics),
            Arc::new(AtomicBool::new(false)),
            None,
            Arc::new(SwitchingFilesystem {
                selected: Arc::clone(&selected),
            }),
        );
        worker
            .panes
            .insert(1, WorkerPane::new(PathBuf::from("project")));

        assert!(worker.process_pane(1), "large unknown A needs another tick");
        assert_eq!(
            shared.lock().expect("shared").panes[&1].state,
            ClaudeState::default()
        );
        while worker.process_pane(1) {}
        assert_eq!(
            shared.lock().expect("shared").panes[&1].state.input_tokens,
            1
        );

        *selected.lock().expect("selected") = Some(path_b.clone());
        force_rescan(&mut worker, 1);
        while worker.process_pane(1) {}
        assert_eq!(
            shared.lock().expect("shared").panes[&1]
                .state
                .model
                .as_deref(),
            Some("claude-sonnet-4-6")
        );

        let appended = "{\"type\":\"assistant\",\"requestId\":\"a-new\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":3}}}\n";
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path_a)
            .expect("open A")
            .write_all(appended.as_bytes())
            .expect("append A");
        metrics.bytes_read.store(0, Ordering::Release);
        *selected.lock().expect("selected") = Some(path_a.clone());
        force_rescan(&mut worker, 1);
        while worker.process_pane(1) {}
        assert_eq!(metrics.bytes_read.load(Ordering::Acquire), appended.len());

        let mut golden = TranscriptMonitor::new();
        apply_to_end(&path_a, &mut golden, usize::MAX);
        assert_eq!(shared.lock().expect("shared").panes[&1].state, golden.state);
        std::fs::remove_file(path_a).expect("remove A");
        std::fs::remove_file(path_b).expect("remove B");
    }

    #[test]
    fn lru_caps_project_and_total_caches_without_evicting_selected_paths() {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let mut worker = MonitorWorker::new(
            shared,
            Arc::new(Mutex::new(WorkerRequests::default())),
            Arc::new(WorkerMetrics::default()),
            Arc::new(AtomicBool::new(false)),
            None,
            Arc::new(RealMonitorFilesystem),
        );
        let selected_path = PathBuf::from("project-0/selected.jsonl");
        worker.panes.insert(
            1,
            WorkerPane {
                cwd: PathBuf::from("project-0"),
                jsonl_path: Some(selected_path.clone()),
                last_rescan: Instant::now(),
            },
        );
        for project_index in 0..9 {
            let cwd = PathBuf::from(format!("project-{project_index}"));
            let cache = worker.projects.entry(cwd).or_default();
            for transcript_index in 0..10 {
                let path = if project_index == 0 && transcript_index == 0 {
                    selected_path.clone()
                } else {
                    PathBuf::from(format!(
                        "project-{project_index}/transcript-{transcript_index}.jsonl"
                    ))
                };
                worker.generation += 1;
                cache.transcripts.insert(
                    path,
                    CachedTranscript {
                        monitor: TranscriptMonitor::new(),
                        last_used: worker.generation,
                    },
                );
            }
        }
        worker.evict_caches();
        assert!(worker
            .projects
            .values()
            .all(|cache| { cache.transcripts.len() <= MAX_TRANSCRIPTS_PER_PROJECT }));
        assert!(worker
            .projects
            .get(Path::new("project-0"))
            .expect("selected project")
            .transcripts
            .contains_key(&selected_path));
        assert!(
            worker
                .projects
                .values()
                .map(|cache| cache.transcripts.len())
                .sum::<usize>()
                <= MAX_TRANSCRIPTS_TOTAL
        );
    }

    #[test]
    fn test_encode_cwd() {
        let path = PathBuf::from(r"C:\Users\foo\bar");
        let encoded = encode_cwd_to_project_name(&path);
        assert_eq!(encoded, "C--Users-foo-bar");
    }

    #[test]
    fn test_encode_cwd_japanese() {
        // Claude encodes non-ASCII chars as dashes too
        let path = PathBuf::from("C:\\Users\\じゅぶ\\dev\\renga");
        let encoded = encode_cwd_to_project_name(&path);
        // C : \ U s e r s \ じ ゅ ぶ \ d e v \ c c m u x
        // C - - Users    - - - - dev - renga
        assert_eq!(encoded, "C--Users-----dev-renga");
    }

    #[test]
    fn test_process_tool_use() {
        let mut monitor = TranscriptMonitor::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","id":"toolu_001","input":{}}],"stop_reason":"tool_use"}}"#;
        process_event(&mut monitor, line);
        assert_eq!(monitor.state.current_tool.as_deref(), Some("Bash"));
        assert!(monitor.state.is_working);
    }

    #[test]
    fn test_process_agent_spawn_and_complete() {
        let mut monitor = TranscriptMonitor::new();

        // Agent (sub-agent) spawn
        let spawn = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Agent","id":"toolu_agent1","input":{}}],"stop_reason":"tool_use"}}"#;
        process_event(&mut monitor, spawn);
        assert_eq!(monitor.state.subagent_count, 1);

        // Sub-agent complete via tool_result
        let complete = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_agent1","content":"done"}]}}"#;
        process_event(&mut monitor, complete);
        assert_eq!(monitor.state.subagent_count, 0);
    }

    #[test]
    fn test_token_usage_no_double_count() {
        let mut monitor = TranscriptMonitor::new();
        // Same requestId appears 3 times (typical Claude JSONL pattern)
        let line = r#"{"type":"assistant","requestId":"req_123","message":{"model":"claude-opus-4-6","content":[{"type":"tool_use","name":"Bash","id":"t1","input":{}}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":1000}}}"#;
        process_event(&mut monitor, line);
        process_event(&mut monitor, line);
        process_event(&mut monitor, line);

        // Should be counted only once
        assert_eq!(monitor.state.input_tokens, 100);
        assert_eq!(monitor.state.output_tokens, 50);
        assert_eq!(monitor.state.cache_read_tokens, 1000);
    }

    #[test]
    fn test_todo_parsing() {
        let mut monitor = TranscriptMonitor::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"TodoWrite","id":"t1","input":{"todos":[{"content":"Task A","status":"completed","activeForm":"Doing A"},{"content":"Task B","status":"in_progress","activeForm":"Doing B"},{"content":"Task C","status":"pending","activeForm":"Doing C"}]}}]}}"#;
        process_event(&mut monitor, line);
        assert_eq!(monitor.state.todos.len(), 3);
        assert_eq!(monitor.state.todo_progress(), (1, 3));
    }

    #[test]
    fn test_stop_reason_end_turn_clears_working() {
        let mut monitor = TranscriptMonitor::new();
        monitor.state.is_working = true;
        monitor.state.current_tool = Some("Bash".to_string());

        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#;
        process_event(&mut monitor, line);
        assert!(!monitor.state.is_working);
        assert!(monitor.state.current_tool.is_none());
    }

    #[test]
    fn test_context_limit_opus_4_6_is_1m() {
        // Claude Code logs the plain model id without the [1m] suffix
        // even though Opus 4.6 ships with 1M context by default.
        let mut state = ClaudeState {
            model: Some("claude-opus-4-6".to_string()),
            ..Default::default()
        };
        assert_eq!(state.context_limit(), 1_000_000);

        // Explicit 1m variant suffix still works.
        state.model = Some("claude-opus-4-6[1m]".to_string());
        assert_eq!(state.context_limit(), 1_000_000);

        // Older Opus uses the 500K baseline.
        state.model = Some("claude-opus-4-5".to_string());
        assert_eq!(state.context_limit(), 500_000);

        // Sonnet / Haiku keep their native 200K window.
        state.model = Some("claude-sonnet-4-6".to_string());
        assert_eq!(state.context_limit(), 200_000);
        state.model = Some("claude-haiku-4-5".to_string());
        assert_eq!(state.context_limit(), 200_000);
    }
}
