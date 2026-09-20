//! Always-on, bounded pane capture.
//!
//! Producers only copy an event into a bounded-accounted, unbounded channel.
//! A per-pane recorder owns the in-memory ring. File work is performed by a
//! separate continuous writer (when the legacy env is set) or by an explicit
//! dump coordinator.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[cfg(test)]
pub(crate) mod replay;

const HOLD_SLACK_BYTES: usize = 1024 * 1024;
const MAX_PTY_READ_BYTES: usize = 4096;
const MIN_AUXILIARY_BYTES: usize = 64 * 1024;
const DUMP_DEADLINE: Duration = Duration::from_secs(3);
const WRITER_QUEUE_RECORDS: usize = 256;
const AUTOMATIC_DUMP_INTERVAL: Duration = Duration::from_secs(10 * 60);

static LIVE_CAPTURES: AtomicUsize = AtomicUsize::new(0);
static FAILURE_COUNT: AtomicU64 = AtomicU64::new(0);
static RECORDER_EXITS: AtomicU64 = AtomicU64::new(0);
static REGISTRY: Mutex<Vec<Weak<Capture>>> = Mutex::new(Vec::new());
static AUTOMATIC_DUMP_PRUNE_LOCK: Mutex<()> = Mutex::new(());
#[cfg(not(test))]
static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct Config {
    /// Unique per-TUI session directory. It is not created without env output
    /// or an explicit dump.
    #[cfg(test)]
    pub(crate) directory: PathBuf,
    pub(crate) dump_root: PathBuf,
    pub(crate) continuous_directory: Option<PathBuf>,
    pub(crate) origin: Instant,
    pub(crate) origin_unix_ms: u128,
    pub(crate) ring_bytes: usize,
    pub(crate) file_bytes: usize,
    pub(crate) file_segments: usize,
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) retained_sessions: usize,
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) total_disk_bytes: usize,
    pub(crate) retained_automatic_dumps: usize,
    pub(crate) automatic_dump_total_bytes: usize,
    #[cfg(test)]
    pub(crate) dump_write_delay: Duration,
    session_token: u128,
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG: std::cell::RefCell<Option<Arc<Config>>> = const {
        std::cell::RefCell::new(None)
    };
}

fn now_since_epoch() -> Option<Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

#[cfg(not(test))]
fn default_dump_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("renga")
        .join("pane-captures")
}

#[cfg(not(test))]
fn resolve_config(
    debug: &crate::config::DebugConfig,
    get_env: impl FnOnce() -> Option<std::ffi::OsString>,
) -> Config {
    let time = now_since_epoch().unwrap_or_default();
    let root = get_env().map(PathBuf::from);
    let dump_root = root.clone().unwrap_or_else(default_dump_root);
    let directory = dump_root.join(format!(
        "session-{}-{}",
        std::process::id(),
        time.as_nanos()
    ));
    Config {
        continuous_directory: root.map(|_| directory.clone()),
        #[cfg(test)]
        directory,
        dump_root,
        origin: Instant::now(),
        origin_unix_ms: time.as_millis(),
        ring_bytes: debug.pane_capture_ring_bytes,
        file_bytes: debug.pane_capture_file_bytes.max(1),
        file_segments: debug.pane_capture_file_segments.max(1),
        retained_sessions: debug.pane_capture_sessions.max(1),
        total_disk_bytes: debug.pane_capture_total_bytes.max(1),
        retained_automatic_dumps: debug.pane_capture_auto_dumps,
        automatic_dump_total_bytes: debug.pane_capture_auto_total_bytes,
        session_token: time.as_nanos(),
    }
}

/// Called once after config is loaded and before the first pane is created.
pub(crate) fn startup(debug: &crate::config::DebugConfig) -> Shutdown {
    #[cfg(not(test))]
    {
        let config = CONFIG
            .get_or_init(|| {
                Arc::new(resolve_config(debug, || {
                    std::env::var_os("RENGA_DEBUG_PANE_CAPTURE")
                }))
            })
            .clone();
        if config.continuous_directory.is_some() {
            prune_old_sessions(&config);
        }
    }
    #[cfg(test)]
    let _ = debug;
    Shutdown
}

pub(crate) fn enabled() -> bool {
    LIVE_CAPTURES.load(Ordering::Relaxed) != 0
}

#[cfg(test)]
pub(crate) fn failure_count() -> u64 {
    FAILURE_COUNT.load(Ordering::Relaxed)
}

pub(crate) struct Shutdown;
impl Drop for Shutdown {
    fn drop(&mut self) {
        let captures: Vec<_> = REGISTRY
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .drain(..)
            .filter_map(|capture| capture.upgrade())
            .collect();
        let deadline = Instant::now() + Duration::from_secs(1);
        for capture in captures {
            capture.flush_until(deadline);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct DeferredState {
    pub(crate) kind: &'static str,
    pub(crate) opened_at_elapsed_us: Option<u64>,
    pub(crate) buffered_len: usize,
}

pub(crate) const DEFERRED_KIND_NONE: &str = "none";
pub(crate) const DEFERRED_KIND_DEC2026: &str = "dec2026";
pub(crate) const DEFERRED_KIND_ERASE_HOLD: &str = "erase_hold";

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Data {
    Metadata {
        version: u8,
        rows: u16,
        cols: u16,
        origin_unix_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        evicted_raw_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        evicted_records: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        first_retained_elapsed_us: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_retained_elapsed_us: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        retained_raw_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        retained_records: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        forced_cuts: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        gaps: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        automatic_dump_suppressions: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        automatic_dump_reason: Option<&'static str>,
    },
    Read {
        bin_offset: u64,
        read_len: usize,
        deferred: DeferredState,
    },
    Transition {
        action: &'static str,
        kind: &'static str,
        marker: Option<&'static str>,
        bin_offset: u64,
        reason: Option<&'static str>,
    },
    ParserApply {
        bin_offset: u64,
        byte_len: usize,
        applied_offset: u64,
    },
    AppTickRelease {
        released_len: usize,
        deferred: DeferredState,
    },
    AppDraw {
        drawn: bool,
        applied_offset: u64,
        scrollback: usize,
        deferred: DeferredState,
    },
    Resize {
        rows: u16,
        cols: u16,
        clear: bool,
        applied_offset: u64,
    },
    ReaderExit {
        deferred: DeferredState,
    },
}

/// Locate erase commands and printable payload runs while skipping terminal
/// control strings. Replay and the live automatic trigger deliberately share
/// this classifier so a cursor move or cursor-hide after an erase is not
/// mistaken for rewritten screen content.
pub(crate) fn repaint_tokens(bytes: &[u8]) -> (Vec<std::ops::Range<usize>>, Vec<usize>) {
    let mut erases = Vec::new();
    let mut payloads = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            let start = index;
            index += 1;
            let Some(&command) = bytes.get(index) else {
                break;
            };
            index += 1;
            match command {
                b'[' => {
                    while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                        index += 1;
                    }
                    if index < bytes.len() {
                        index += 1;
                    }
                    if matches!(&bytes[start..index], b"\x1b[2J" | b"\x1b[3J") {
                        erases.push(start..index);
                    }
                }
                b']' | b'P' | b'X' | b'^' | b'_' => {
                    while index < bytes.len() {
                        if command == b']' && bytes[index] == 7 {
                            index += 1;
                            break;
                        }
                        if bytes[index..].starts_with(b"\x1b\\") {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                0x20..=0x2f => {
                    while index < bytes.len() && (0x20..=0x2f).contains(&bytes[index]) {
                        index += 1;
                    }
                    if index < bytes.len() {
                        index += 1;
                    }
                }
                _ => {}
            }
        } else if bytes[index] >= 0x20 && bytes[index] != 0x7f {
            payloads.push(index);
            while index < bytes.len() && bytes[index] >= 0x20 && bytes[index] != 0x7f {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    (erases, payloads)
}

pub(crate) fn erase_hold_closed_before_rewrite(bytes: &[u8]) -> bool {
    let (erases, payloads) = repaint_tokens(bytes);
    let Some(first_erase) = erases.first() else {
        return false;
    };
    !payloads.iter().any(|offset| *offset >= first_erase.end)
}

#[derive(Clone, Serialize)]
struct Record {
    sequence: u64,
    timestamp_unix_ms: u128,
    elapsed_us: u64,
    pane_id: usize,
    process_id: u32,
    child_process_id: Option<u32>,
    #[serde(flatten)]
    data: Data,
}

#[derive(Clone)]
struct StoredRecord {
    timestamp_unix_ms: u128,
    elapsed_us: u64,
    data: Data,
    bytes: Option<Vec<u8>>,
    cost: usize,
}

// Boxing the record variant would add an allocation to every captured event.
// The channel stores commands out of line already, so retain the hot-path
// representation even though the administrative variants are much smaller.
#[allow(clippy::large_enum_variant)]
enum RecorderCommand {
    Record(StoredRecord),
    AutomaticDump {
        elapsed_us: u64,
        reason: &'static str,
    },
    Snapshot(mpsc::Sender<RingSnapshot>),
    Flush(mpsc::Sender<()>),
    #[cfg(test)]
    Block {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    },
}

#[allow(clippy::large_enum_variant)]
enum DiskCommand {
    Record(StoredRecord),
    Flush(mpsc::Sender<()>),
}

#[derive(Clone)]
struct RingSnapshot {
    rows: u16,
    cols: u16,
    base: u64,
    raw_end: u64,
    records: Vec<StoredRecord>,
    evicted_raw_bytes: u64,
    evicted_records: u64,
    forced_cuts: u64,
    gaps: u64,
    automatic_dump_suppressions: u64,
}

struct RingState {
    rows: u16,
    cols: u16,
    base: u64,
    raw_end: u64,
    records: VecDeque<StoredRecord>,
    record_cost: usize,
    raw_bytes: usize,
    cap: usize,
    record_cap: usize,
    hard_cap: usize,
    evicted_raw_bytes: u64,
    evicted_records: u64,
    forced_cuts: u64,
    gaps: u64,
    automatic_dump_suppressions: u64,
}

impl RingState {
    fn new(rows: u16, cols: u16, cap: usize) -> Self {
        Self {
            rows,
            cols,
            base: 0,
            raw_end: 0,
            records: VecDeque::new(),
            record_cost: 0,
            raw_bytes: 0,
            cap,
            record_cap: cap.max(MIN_AUXILIARY_BYTES),
            hard_cap: cap
                .saturating_add(HOLD_SLACK_BYTES)
                .saturating_add(MAX_PTY_READ_BYTES),
            evicted_raw_bytes: 0,
            evicted_records: 0,
            forced_cuts: 0,
            gaps: 0,
            automatic_dump_suppressions: 0,
        }
    }

    fn push(&mut self, record: StoredRecord, dropped: u64) -> bool {
        let forced_before = self.forced_cuts;
        self.gaps = self.gaps.saturating_add(dropped);
        if let Data::Read {
            bin_offset,
            read_len,
            ..
        } = &record.data
        {
            self.raw_end = (*bin_offset).saturating_add(*read_len as u64);
            self.raw_bytes = self.raw_bytes.saturating_add(*read_len);
        }
        self.record_cost = self.record_cost.saturating_add(record.cost);
        self.records.push_back(record);
        self.trim_print_only();
        self.evict_if_needed();
        self.forced_cuts != forced_before
    }

    fn trim_print_only(&mut self) {
        while self.record_cost > self.record_cap {
            let Some(index) = self.records.iter().position(|record| {
                matches!(
                    record.data,
                    Data::AppDraw { .. } | Data::AppTickRelease { .. } | Data::ReaderExit { .. }
                )
            }) else {
                break;
            };
            if let Some(record) = self.records.remove(index) {
                self.record_cost = self.record_cost.saturating_sub(record.cost);
                self.evicted_records = self.evicted_records.saturating_add(1);
            }
        }
    }

    fn evict_if_needed(&mut self) {
        if self.raw_bytes <= self.cap {
            return;
        }
        let desired = self.raw_end.saturating_sub(self.cap as u64);
        if let Some(cut) = self.safe_cut_at_or_after(desired) {
            self.cut(cut, false);
            return;
        }
        if self.raw_bytes > self.hard_cap {
            let hard_floor = self.raw_end.saturating_sub(self.hard_cap as u64);
            if let Some(cut) = self
                .read_start_at_or_after(hard_floor)
                .filter(|cut| *cut > self.base)
            {
                self.cut(cut, true);
            }
        }
    }

    fn read_start_at_or_after(&self, target: u64) -> Option<u64> {
        self.records.iter().find_map(|record| match record.data {
            Data::Read { bin_offset, .. } if bin_offset >= target => Some(bin_offset),
            _ => None,
        })
    }

    fn safe_cut_at_or_after(&self, target: u64) -> Option<u64> {
        let mut applied = self.base;
        let mut prior_deferred = DEFERRED_KIND_NONE;
        for record in &self.records {
            match &record.data {
                Data::ParserApply { applied_offset, .. } => applied = *applied_offset,
                Data::Read {
                    bin_offset,
                    deferred,
                    ..
                } => {
                    if *bin_offset >= target
                        && *bin_offset > self.base
                        && prior_deferred == DEFERRED_KIND_NONE
                        && applied >= *bin_offset
                    {
                        return Some(*bin_offset);
                    }
                    prior_deferred = deferred.kind;
                }
                _ => {}
            }
        }
        None
    }

    fn cut(&mut self, cut: u64, forced: bool) {
        if cut <= self.base || cut > self.raw_end {
            return;
        }
        let first_read = self.records.iter().position(
            |record| matches!(record.data, Data::Read { bin_offset, .. } if bin_offset == cut),
        );
        let Some(first_read) = first_read else {
            return;
        };
        for record in self.records.iter().take(first_read) {
            if let Data::Resize { rows, cols, .. } = record.data {
                self.rows = rows;
                self.cols = cols;
            }
        }
        for _ in 0..first_read {
            if let Some(record) = self.records.pop_front() {
                self.record_cost = self.record_cost.saturating_sub(record.cost);
                self.evicted_records = self.evicted_records.saturating_add(1);
            }
        }
        self.records.retain_mut(|record| match &mut record.data {
            Data::ParserApply {
                bin_offset,
                byte_len,
                applied_offset,
            } => {
                if *applied_offset <= cut {
                    false
                } else {
                    if *bin_offset < cut {
                        *bin_offset = cut;
                        *byte_len = (*applied_offset - cut) as usize;
                    }
                    true
                }
            }
            Data::Transition { bin_offset, .. } => *bin_offset >= cut,
            Data::AppDraw { applied_offset, .. } | Data::Resize { applied_offset, .. } => {
                *applied_offset >= cut
            }
            _ => true,
        });
        self.record_cost = self.records.iter().map(|record| record.cost).sum();
        let removed = cut - self.base;
        self.base = cut;
        self.raw_bytes = self.raw_end.saturating_sub(cut) as usize;
        self.evicted_raw_bytes = self.evicted_raw_bytes.saturating_add(removed);
        if forced {
            self.forced_cuts = self.forced_cuts.saturating_add(1);
        }
    }

    fn snapshot(&self) -> RingSnapshot {
        RingSnapshot {
            rows: self.rows,
            cols: self.cols,
            base: self.base,
            raw_end: self.raw_end,
            records: self.records.iter().cloned().collect(),
            evicted_raw_bytes: self.evicted_raw_bytes,
            evicted_records: self.evicted_records,
            forced_cuts: self.forced_cuts,
            gaps: self.gaps,
            automatic_dump_suppressions: self.automatic_dump_suppressions,
        }
    }
}

#[derive(Default)]
struct AutomaticDumpLimiter {
    last_fired_elapsed_us: Option<u64>,
}

impl AutomaticDumpLimiter {
    fn admit(&mut self, elapsed_us: u64) -> bool {
        let interval_us = AUTOMATIC_DUMP_INTERVAL.as_micros() as u64;
        if self
            .last_fired_elapsed_us
            .is_some_and(|last| elapsed_us.saturating_sub(last) < interval_us)
        {
            return false;
        }
        self.last_fired_elapsed_us = Some(elapsed_us);
        true
    }
}

pub(crate) struct Capture {
    config: Arc<Config>,
    pane_id: usize,
    child_process_id: Option<u32>,
    sender: mpsc::Sender<RecorderCommand>,
    queued_bytes: Arc<AtomicUsize>,
    dropped_records: Arc<AtomicU64>,
    recorder_alive: Arc<AtomicBool>,
    applied_offset: AtomicU64,
    deferred_kind: AtomicUsize,
    deferred_opened: AtomicU64,
    deferred_buffered: AtomicUsize,
    drawn_this_frame: AtomicBool,
    dump_in_flight: AtomicBool,
}

impl Drop for Capture {
    fn drop(&mut self) {
        LIVE_CAPTURES.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Capture {
    pub(crate) fn for_pane(
        pane_id: usize,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
    ) -> Option<Arc<Self>> {
        #[cfg(test)]
        let config = TEST_CONFIG.with(|value| value.borrow().clone());
        #[cfg(not(test))]
        let config = CONFIG.get().cloned();
        config.and_then(|config| Self::create_arc(config, pane_id, child_pid, rows, cols))
    }

    #[cfg(test)]
    pub(crate) fn create(
        config: Config,
        pane_id: usize,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
    ) -> Option<Arc<Self>> {
        Self::create_arc(Arc::new(config), pane_id, child_pid, rows, cols)
    }

    fn create_arc(
        config: Arc<Config>,
        pane_id: usize,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
    ) -> Option<Arc<Self>> {
        if config.ring_bytes == 0 {
            return None;
        }
        let (sender, receiver) = mpsc::channel();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let dropped_records = Arc::new(AtomicU64::new(0));
        let recorder_alive = Arc::new(AtomicBool::new(true));
        let worker_queued = queued_bytes.clone();
        let worker_dropped = dropped_records.clone();
        let worker_alive = recorder_alive.clone();
        let worker_config = config.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("capture-ring-{pane_id}"))
            .spawn(move || {
                recorder_loop(
                    worker_config,
                    pane_id,
                    child_pid,
                    rows,
                    cols,
                    receiver,
                    worker_queued,
                    worker_dropped,
                );
                worker_alive.store(false, Ordering::Release);
                RECORDER_EXITS.fetch_add(1, Ordering::Relaxed);
            });
        if spawn.is_err() {
            FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let capture = Arc::new(Self {
            config,
            pane_id,
            child_process_id: child_pid,
            sender,
            queued_bytes,
            dropped_records,
            recorder_alive,
            applied_offset: AtomicU64::new(0),
            deferred_kind: AtomicUsize::new(0),
            deferred_opened: AtomicU64::new(u64::MAX),
            deferred_buffered: AtomicUsize::new(0),
            drawn_this_frame: AtomicBool::new(false),
            dump_in_flight: AtomicBool::new(false),
        });
        LIVE_CAPTURES.fetch_add(1, Ordering::Relaxed);
        REGISTRY
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .push(Arc::downgrade(&capture));
        Some(capture)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.recorder_alive.load(Ordering::Acquire)
    }

    pub(crate) fn elapsed_us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.config.origin)
            .as_micros()
            .min(u64::MAX as u128) as u64
    }

    fn enqueue(&self, at: Instant, data: Data, bytes: Option<Vec<u8>>) {
        if !self.is_enabled() {
            return;
        }
        let cost = bytes.as_ref().map_or(0, Vec::len) + std::mem::size_of::<StoredRecord>() + 64;
        let backlog_cap = self.config.ring_bytes.max(MIN_AUXILIARY_BYTES);
        let charged =
            self.queued_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current
                        .checked_add(cost)
                        .filter(|next| *next <= backlog_cap)
                });
        if charged.is_err() {
            self.dropped_records.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let elapsed_us = self.elapsed_us(at);
        let record = StoredRecord {
            timestamp_unix_ms: self.config.origin_unix_ms + u128::from(elapsed_us / 1000),
            elapsed_us,
            data,
            bytes,
            cost,
        };
        if self.sender.send(RecorderCommand::Record(record)).is_err() {
            self.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
        }
    }

    pub(crate) fn event(&self, at: Instant, data: Data, _flush: bool) {
        self.enqueue(at, data, None);
    }

    pub(crate) fn automatic_dump(&self, at: Instant, reason: &'static str) {
        if !self.is_enabled() {
            return;
        }
        let _ = self.sender.send(RecorderCommand::AutomaticDump {
            elapsed_us: self.elapsed_us(at),
            reason,
        });
    }

    pub(crate) fn read(&self, at: Instant, offset: u64, bytes: &[u8], deferred: DeferredState) {
        self.set_deferred(deferred.clone());
        self.enqueue(
            at,
            Data::Read {
                bin_offset: offset,
                read_len: bytes.len(),
                deferred,
            },
            Some(bytes.to_vec()),
        );
    }

    pub(crate) fn set_deferred(&self, deferred: DeferredState) {
        let kind = match deferred.kind {
            DEFERRED_KIND_DEC2026 => 1,
            DEFERRED_KIND_ERASE_HOLD => 2,
            _ => 0,
        };
        self.deferred_kind.store(kind, Ordering::Release);
        self.deferred_opened.store(
            deferred.opened_at_elapsed_us.unwrap_or(u64::MAX),
            Ordering::Release,
        );
        self.deferred_buffered
            .store(deferred.buffered_len, Ordering::Release);
    }

    fn deferred(&self) -> DeferredState {
        DeferredState {
            kind: match self.deferred_kind.load(Ordering::Acquire) {
                1 => DEFERRED_KIND_DEC2026,
                2 => DEFERRED_KIND_ERASE_HOLD,
                _ => DEFERRED_KIND_NONE,
            },
            opened_at_elapsed_us: match self.deferred_opened.load(Ordering::Acquire) {
                u64::MAX => None,
                value => Some(value),
            },
            buffered_len: self.deferred_buffered.load(Ordering::Acquire),
        }
    }

    /// Caller holds the parser lock across mutation and this send.
    pub(crate) fn applied(&self, byte_len: usize) {
        let bin_offset = self
            .applied_offset
            .fetch_add(byte_len as u64, Ordering::AcqRel);
        self.enqueue(
            Instant::now(),
            Data::ParserApply {
                bin_offset,
                byte_len,
                applied_offset: bin_offset + byte_len as u64,
            },
            None,
        );
    }

    /// Caller holds the parser lock. Injected clear bytes are not PTY bytes.
    pub(crate) fn resize(&self, rows: u16, cols: u16) {
        self.enqueue(
            Instant::now(),
            Data::Resize {
                rows,
                cols,
                clear: true,
                applied_offset: self.applied_offset.load(Ordering::Acquire),
            },
            None,
        );
    }

    pub(crate) fn finish_update(&self) {}

    pub(crate) fn draw(&self, drawn: bool, scrollback: usize) {
        if drawn {
            self.drawn_this_frame.store(true, Ordering::Relaxed);
        }
        self.enqueue(
            Instant::now(),
            Data::AppDraw {
                drawn,
                scrollback,
                applied_offset: self.applied_offset.load(Ordering::Acquire),
                deferred: self.deferred(),
            },
            None,
        );
    }

    pub(crate) fn finish_draw(&self) {
        if !self.drawn_this_frame.swap(false, Ordering::Relaxed) {
            self.draw(false, 0);
        }
    }

    fn flush_until(&self, deadline: Instant) {
        let (done, receive) = mpsc::channel();
        if self.sender.send(RecorderCommand::Flush(done)).is_ok() {
            let _ = receive.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }

    #[cfg(test)]
    pub(crate) fn flush(&self) {
        self.flush_until(Instant::now() + Duration::from_secs(1));
    }

    fn snapshot_until(&self, deadline: Instant) -> Result<RingSnapshot, &'static str> {
        if self
            .dump_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("dump already in flight");
        }
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _reset = Reset(&self.dump_in_flight);
        let (reply, receive) = mpsc::channel();
        self.sender
            .send(RecorderCommand::Snapshot(reply))
            .map_err(|_| "recorder stopped")?;
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "snapshot timed out")
    }
}

#[allow(clippy::too_many_arguments)]
fn recorder_loop(
    config: Arc<Config>,
    pane_id: usize,
    child_process_id: Option<u32>,
    rows: u16,
    cols: u16,
    receiver: mpsc::Receiver<RecorderCommand>,
    queued_bytes: Arc<AtomicUsize>,
    dropped_records: Arc<AtomicU64>,
) {
    let disk = config.continuous_directory.as_ref().and_then(|directory| {
        spawn_disk_writer(
            config.clone(),
            directory.clone(),
            pane_id,
            child_process_id,
            rows,
            cols,
        )
    });
    let mut ring = RingState::new(rows, cols, config.ring_bytes);
    let mut automatic_limiter = AutomaticDumpLimiter::default();
    let mut automatic_sequence = 0_u64;
    while let Ok(command) = receiver.recv() {
        match command {
            RecorderCommand::Record(record) => {
                let elapsed_us = record.elapsed_us;
                queued_bytes.fetch_sub(record.cost, Ordering::AcqRel);
                let dropped = dropped_records.swap(0, Ordering::AcqRel);
                if let Some(sender) = &disk {
                    if sender
                        .try_send(DiskCommand::Record(record.clone()))
                        .is_err()
                    {
                        ring.gaps = ring.gaps.saturating_add(1);
                    }
                }
                let forced_cut = ring.push(record, dropped);
                if forced_cut {
                    schedule_automatic_dump(
                        config.clone(),
                        pane_id,
                        child_process_id,
                        &mut ring,
                        &mut automatic_limiter,
                        &mut automatic_sequence,
                        elapsed_us,
                        "forced_cut",
                    );
                }
            }
            RecorderCommand::AutomaticDump { elapsed_us, reason } => {
                schedule_automatic_dump(
                    config.clone(),
                    pane_id,
                    child_process_id,
                    &mut ring,
                    &mut automatic_limiter,
                    &mut automatic_sequence,
                    elapsed_us,
                    reason,
                );
            }
            RecorderCommand::Snapshot(reply) => {
                ring.gaps = ring
                    .gaps
                    .saturating_add(dropped_records.swap(0, Ordering::AcqRel));
                let _ = reply.send(ring.snapshot());
            }
            RecorderCommand::Flush(done) => {
                if let Some(sender) = &disk {
                    let (disk_done, disk_receive) = mpsc::channel();
                    let _ = sender.try_send(DiskCommand::Flush(disk_done));
                    let _ = disk_receive.recv_timeout(Duration::from_millis(900));
                }
                let _ = done.send(());
            }
            #[cfg(test)]
            RecorderCommand::Block { entered, release } => {
                let _ = entered.send(());
                let _ = release.recv();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn schedule_automatic_dump(
    config: Arc<Config>,
    pane_id: usize,
    child_process_id: Option<u32>,
    ring: &mut RingState,
    limiter: &mut AutomaticDumpLimiter,
    sequence: &mut u64,
    elapsed_us: u64,
    reason: &'static str,
) {
    if config.retained_automatic_dumps == 0
        || config.automatic_dump_total_bytes == 0
        || !limiter.admit(elapsed_us)
    {
        ring.automatic_dump_suppressions = ring.automatic_dump_suppressions.saturating_add(1);
        return;
    }
    let snapshot = ring.snapshot();
    let current_sequence = *sequence;
    *sequence = sequence.saturating_add(1);
    let spawn = std::thread::Builder::new()
        .name(format!("capture-auto-{pane_id}"))
        .spawn(move || {
            if write_automatic_snapshot(
                &config,
                pane_id,
                child_process_id,
                current_sequence,
                reason,
                snapshot,
            )
            .is_err()
            {
                FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        });
    if spawn.is_err() {
        FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

fn spawn_disk_writer(
    config: Arc<Config>,
    directory: PathBuf,
    pane_id: usize,
    child_process_id: Option<u32>,
    rows: u16,
    cols: u16,
) -> Option<mpsc::SyncSender<DiskCommand>> {
    let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_RECORDS);
    let spawn = std::thread::Builder::new()
        .name(format!("capture-disk-{pane_id}"))
        .spawn(move || {
            if continuous_writer_loop(
                &config,
                &directory,
                pane_id,
                child_process_id,
                rows,
                cols,
                receiver,
            )
            .is_err()
            {
                FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        });
    spawn.ok().map(|_| sender)
}

struct SegmentWriter {
    binary: BufWriter<File>,
    jsonl: BufWriter<File>,
    bin_path: PathBuf,
    jsonl_path: PathBuf,
    sequence: u64,
    raw_written: usize,
}

#[allow(clippy::too_many_arguments)]
fn open_segment(
    directory: &Path,
    pane_id: usize,
    part: u64,
    base: u64,
    rows: u16,
    cols: u16,
    config: &Config,
    child_process_id: Option<u32>,
) -> std::io::Result<SegmentWriter> {
    std::fs::create_dir_all(directory)?;
    let stem = if part == 0 {
        format!("pane-{pane_id}")
    } else {
        format!("pane-{pane_id}-part-{part}")
    };
    let bin_path = directory.join(format!("{stem}.bin"));
    let jsonl_path = directory.join(format!("{stem}.jsonl"));
    let mut writer = SegmentWriter {
        binary: BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&bin_path)?,
        ),
        jsonl: BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&jsonl_path)?,
        ),
        bin_path,
        jsonl_path,
        sequence: 0,
        raw_written: 0,
    };
    let metadata = Data::Metadata {
        version: 1,
        rows,
        cols,
        origin_unix_ms: config.origin_unix_ms,
        evicted_raw_bytes: Some(base),
        evicted_records: Some(0),
        first_retained_elapsed_us: None,
        last_retained_elapsed_us: None,
        retained_raw_bytes: None,
        retained_records: None,
        forced_cuts: Some(0),
        gaps: Some(0),
        automatic_dump_suppressions: Some(0),
        automatic_dump_reason: None,
    };
    writer.write_record(config, pane_id, child_process_id, 0, metadata, None)?;
    Ok(writer)
}

impl SegmentWriter {
    fn write_record(
        &mut self,
        config: &Config,
        pane_id: usize,
        child_process_id: Option<u32>,
        elapsed_us: u64,
        data: Data,
        bytes: Option<&[u8]>,
    ) -> std::io::Result<()> {
        if let Some(bytes) = bytes {
            self.binary.write_all(bytes)?;
            self.raw_written = self.raw_written.saturating_add(bytes.len());
        }
        let record = Record {
            sequence: self.sequence,
            timestamp_unix_ms: config.origin_unix_ms + u128::from(elapsed_us / 1000),
            elapsed_us,
            pane_id,
            process_id: std::process::id(),
            child_process_id,
            data,
        };
        serde_json::to_writer(&mut self.jsonl, &record)?;
        self.jsonl.write_all(b"\n")?;
        self.sequence += 1;
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.binary.flush()?;
        self.jsonl.flush()
    }
}

fn continuous_writer_loop(
    config: &Config,
    directory: &Path,
    pane_id: usize,
    child_process_id: Option<u32>,
    mut rows: u16,
    mut cols: u16,
    receiver: mpsc::Receiver<DiskCommand>,
) -> std::io::Result<()> {
    let mut part = 0;
    let mut base = 0;
    let mut applied = 0;
    let mut deferred_kind = DEFERRED_KIND_NONE;
    let mut writer = open_segment(
        directory,
        pane_id,
        part,
        base,
        rows,
        cols,
        config,
        child_process_id,
    )?;
    let mut segments = VecDeque::<(PathBuf, PathBuf)>::new();
    while let Ok(command) = receiver.recv() {
        match command {
            DiskCommand::Record(record) => {
                if let Data::Read { bin_offset, .. } = &record.data {
                    let safe = deferred_kind == DEFERRED_KIND_NONE && applied >= *bin_offset;
                    let hard = writer.raw_written
                        > config
                            .file_bytes
                            .saturating_add(HOLD_SLACK_BYTES)
                            .saturating_add(MAX_PTY_READ_BYTES);
                    if writer.raw_written >= config.file_bytes && (safe || hard) {
                        writer.flush()?;
                        segments.push_back((writer.bin_path.clone(), writer.jsonl_path.clone()));
                        while segments.len() >= config.file_segments {
                            if let Some((bin, jsonl)) = segments.pop_front() {
                                let _ = std::fs::remove_file(bin);
                                let _ = std::fs::remove_file(jsonl);
                            }
                        }
                        part += 1;
                        base = *bin_offset;
                        writer = open_segment(
                            directory,
                            pane_id,
                            part,
                            base,
                            rows,
                            cols,
                            config,
                            child_process_id,
                        )?;
                    }
                }
                if let Data::Resize {
                    rows: next_rows,
                    cols: next_cols,
                    ..
                } = &record.data
                {
                    rows = *next_rows;
                    cols = *next_cols;
                }
                if let Data::Read { deferred, .. } = &record.data {
                    deferred_kind = deferred.kind;
                }
                if let Data::ParserApply { applied_offset, .. } = &record.data {
                    applied = *applied_offset;
                }
                if let Some(data) = rebase_data(record.data.clone(), base) {
                    writer.write_record(
                        config,
                        pane_id,
                        child_process_id,
                        record.elapsed_us,
                        data,
                        record.bytes.as_deref(),
                    )?;
                }
            }
            DiskCommand::Flush(done) => {
                let result = writer.flush();
                let _ = done.send(());
                result?;
            }
        }
    }
    writer.flush()
}

fn rebase_data(mut data: Data, base: u64) -> Option<Data> {
    match &mut data {
        Data::Metadata { .. } => return None,
        Data::Read { bin_offset, .. } => {
            if *bin_offset < base {
                return None;
            }
            *bin_offset -= base;
        }
        Data::Transition { bin_offset, .. } => {
            if *bin_offset < base {
                return None;
            }
            *bin_offset -= base;
        }
        Data::ParserApply {
            bin_offset,
            byte_len,
            applied_offset,
        } => {
            if *applied_offset <= base {
                return None;
            }
            let start = (*bin_offset).max(base);
            *bin_offset = start - base;
            *byte_len = (*applied_offset - start) as usize;
            *applied_offset -= base;
        }
        Data::AppDraw { applied_offset, .. } | Data::Resize { applied_offset, .. } => {
            if *applied_offset < base {
                return None;
            }
            *applied_offset -= base;
        }
        Data::AppTickRelease { .. } | Data::ReaderExit { .. } => {}
    }
    Some(data)
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PaneDumpReport {
    pub(crate) pane_id: usize,
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bin_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) jsonl_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    pub(crate) retained_raw_bytes: u64,
    pub(crate) retained_records: u64,
    pub(crate) first_retained_elapsed_us: Option<u64>,
    pub(crate) last_retained_elapsed_us: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DumpReport {
    pub(crate) directory: String,
    pub(crate) panes: Vec<PaneDumpReport>,
}

pub(crate) fn dump_captures_async(
    captures: Vec<Arc<Capture>>,
    done: impl FnOnce(Result<DumpReport, String>) + Send + 'static,
) -> Result<(), String> {
    let spawn = std::thread::Builder::new()
        .name("pane-capture-dump".into())
        .spawn(move || done(dump_captures(captures)));
    if spawn.is_err() {
        FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
        return Err("cannot start pane capture dump worker".into());
    }
    Ok(())
}

pub(crate) fn format_dump_report(report: &DumpReport) -> String {
    let ok = report
        .panes
        .iter()
        .filter(|pane| pane.status == "ok")
        .count();
    let failed = report.panes.len().saturating_sub(ok);
    if failed == 0 {
        format!("saved {ok} pane(s) to {}", report.directory)
    } else {
        format!(
            "saved {ok} pane(s), {failed} failed; directory {}",
            report.directory
        )
    }
}

pub(crate) fn dump_captures(captures: Vec<Arc<Capture>>) -> Result<DumpReport, String> {
    let first = captures.first().ok_or("no panes selected")?;
    let stamp = now_since_epoch().unwrap_or_default().as_nanos();
    std::fs::create_dir_all(&first.config.dump_root).map_err(|error| {
        format!(
            "cannot create {}: {error}",
            first.config.dump_root.display()
        )
    })?;
    let directory = first
        .config
        .dump_root
        .join(format!("manual-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let deadline = Instant::now() + DUMP_DEADLINE;
    let mut panes = Vec::with_capacity(captures.len());
    for capture in captures {
        if Instant::now() >= deadline {
            panes.push(failed_dump(
                capture.pane_id,
                "timed_out",
                "dump deadline exceeded",
            ));
            continue;
        }
        match capture.snapshot_until(deadline) {
            Ok(snapshot) => match write_snapshot_until(
                directory.clone(),
                capture.config.clone(),
                capture.pane_id,
                capture.child_process_id,
                snapshot,
                deadline,
            ) {
                Ok(report) => panes.push(report),
                Err(DeadlineWriteError::TimedOut) => panes.push(failed_dump(
                    capture.pane_id,
                    "timed_out",
                    "dump deadline exceeded",
                )),
                Err(DeadlineWriteError::Failed(error)) => {
                    panes.push(failed_dump(capture.pane_id, "failed", &error))
                }
            },
            Err(reason) => panes.push(failed_dump(
                capture.pane_id,
                if reason.contains("timed out") {
                    "timed_out"
                } else {
                    "failed"
                },
                reason,
            )),
        }
    }
    Ok(DumpReport {
        directory: directory.to_string_lossy().into_owned(),
        panes,
    })
}

enum DeadlineWriteError {
    TimedOut,
    Failed(String),
}

fn write_snapshot_until(
    directory: PathBuf,
    config: Arc<Config>,
    pane_id: usize,
    child_process_id: Option<u32>,
    snapshot: RingSnapshot,
    deadline: Instant,
) -> Result<PaneDumpReport, DeadlineWriteError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(DeadlineWriteError::TimedOut);
    }
    let staging_directory = directory.join(format!(".pane-{pane_id}-pending"));
    let worker_staging_directory = staging_directory.clone();
    let (done, receive) = mpsc::sync_channel(0);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("capture-dump-write-{pane_id}"))
        .spawn(move || {
            #[cfg(test)]
            {
                let delay_deadline = Instant::now() + config.dump_write_delay;
                while Instant::now() < delay_deadline {
                    if worker_cancelled.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(
                        Duration::from_millis(10)
                            .min(delay_deadline.saturating_duration_since(Instant::now())),
                    );
                }
            }
            if worker_cancelled.load(Ordering::Acquire) {
                return;
            }
            let result = std::fs::create_dir(&worker_staging_directory)
                .and_then(|()| {
                    write_snapshot(
                        &worker_staging_directory,
                        &config,
                        pane_id,
                        child_process_id,
                        snapshot,
                        None,
                    )
                })
                .map_err(|error| error.to_string());
            let failed = result.is_err();
            let send_failed = done.send(result).is_err();
            if failed || send_failed {
                let _ = std::fs::remove_dir_all(&worker_staging_directory);
            }
        });
    if let Err(error) = spawn {
        return Err(DeadlineWriteError::Failed(error.to_string()));
    }
    match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(mut report)) => {
            if Instant::now() >= deadline {
                cancelled.store(true, Ordering::Release);
                let _ = std::fs::remove_dir_all(&staging_directory);
                return Err(DeadlineWriteError::TimedOut);
            }
            let staged_bin = PathBuf::from(report.bin_path.as_deref().unwrap_or_default());
            let staged_jsonl = PathBuf::from(report.jsonl_path.as_deref().unwrap_or_default());
            let bin_path = directory.join(format!("pane-{pane_id}.bin"));
            let jsonl_path = directory.join(format!("pane-{pane_id}.jsonl"));
            if let Err(error) = std::fs::rename(&staged_bin, &bin_path) {
                let _ = std::fs::remove_dir_all(&staging_directory);
                return Err(DeadlineWriteError::Failed(error.to_string()));
            }
            if let Err(error) = std::fs::rename(&staged_jsonl, &jsonl_path) {
                let _ = std::fs::remove_file(&bin_path);
                let _ = std::fs::remove_dir_all(&staging_directory);
                return Err(DeadlineWriteError::Failed(error.to_string()));
            }
            let _ = std::fs::remove_dir(&staging_directory);
            report.bin_path = Some(bin_path.to_string_lossy().into_owned());
            report.jsonl_path = Some(jsonl_path.to_string_lossy().into_owned());
            Ok(report)
        }
        Ok(Err(error)) => Err(DeadlineWriteError::Failed(error)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            cancelled.store(true, Ordering::Release);
            Err(DeadlineWriteError::TimedOut)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(DeadlineWriteError::Failed("dump writer stopped".to_owned()))
        }
    }
}

fn failed_dump(pane_id: usize, status: &'static str, reason: &str) -> PaneDumpReport {
    PaneDumpReport {
        pane_id,
        status,
        bin_path: None,
        jsonl_path: None,
        reason: Some(reason.to_owned()),
        retained_raw_bytes: 0,
        retained_records: 0,
        first_retained_elapsed_us: None,
        last_retained_elapsed_us: None,
    }
}

fn write_snapshot(
    directory: &Path,
    config: &Config,
    pane_id: usize,
    child_process_id: Option<u32>,
    snapshot: RingSnapshot,
    automatic_reason: Option<&'static str>,
) -> std::io::Result<PaneDumpReport> {
    let bin_path = directory.join(format!("pane-{pane_id}.bin"));
    let jsonl_path = directory.join(format!("pane-{pane_id}.jsonl"));
    let mut binary = BufWriter::new(
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&bin_path)?,
    );
    let mut jsonl = BufWriter::new(
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&jsonl_path)?,
    );
    let first_elapsed = snapshot.records.first().map(|record| record.elapsed_us);
    let last_elapsed = snapshot.records.last().map(|record| record.elapsed_us);
    let retained_records = snapshot.records.len() as u64;
    let retained_raw = snapshot.raw_end.saturating_sub(snapshot.base);
    let metadata = Data::Metadata {
        version: 1,
        rows: snapshot.rows,
        cols: snapshot.cols,
        origin_unix_ms: config.origin_unix_ms,
        evicted_raw_bytes: Some(snapshot.evicted_raw_bytes),
        evicted_records: Some(snapshot.evicted_records),
        first_retained_elapsed_us: first_elapsed,
        last_retained_elapsed_us: last_elapsed,
        retained_raw_bytes: Some(retained_raw),
        retained_records: Some(retained_records),
        forced_cuts: Some(snapshot.forced_cuts),
        gaps: Some(snapshot.gaps),
        automatic_dump_suppressions: Some(snapshot.automatic_dump_suppressions),
        automatic_dump_reason: automatic_reason,
    };
    let mut sequence = 0;
    write_json_record(
        &mut jsonl,
        pane_id,
        child_process_id,
        sequence,
        0,
        config.origin_unix_ms,
        metadata,
    )?;
    sequence += 1;
    for record in snapshot.records {
        let Some(data) = rebase_data(record.data, snapshot.base) else {
            continue;
        };
        if let Some(bytes) = record.bytes.as_deref() {
            binary.write_all(bytes)?;
        }
        write_json_record(
            &mut jsonl,
            pane_id,
            child_process_id,
            sequence,
            record.elapsed_us,
            record.timestamp_unix_ms,
            data,
        )?;
        sequence += 1;
    }
    binary.flush()?;
    jsonl.flush()?;
    Ok(PaneDumpReport {
        pane_id,
        status: "ok",
        bin_path: Some(bin_path.to_string_lossy().into_owned()),
        jsonl_path: Some(jsonl_path.to_string_lossy().into_owned()),
        reason: None,
        retained_raw_bytes: retained_raw,
        retained_records: sequence,
        first_retained_elapsed_us: first_elapsed,
        last_retained_elapsed_us: last_elapsed,
    })
}

fn write_json_record(
    writer: &mut BufWriter<File>,
    pane_id: usize,
    child_process_id: Option<u32>,
    sequence: u64,
    elapsed_us: u64,
    timestamp_unix_ms: u128,
    data: Data,
) -> std::io::Result<()> {
    let record = Record {
        sequence,
        timestamp_unix_ms,
        elapsed_us,
        pane_id,
        process_id: std::process::id(),
        child_process_id,
        data,
    };
    serde_json::to_writer(&mut *writer, &record)?;
    writer.write_all(b"\n")
}

const AUTOMATIC_DUMP_MARKER: &str = ".renga-auto-pane-capture-v1";
const AUTOMATIC_DUMP_COMPLETE: &str = ".complete";

fn write_automatic_snapshot(
    config: &Config,
    pane_id: usize,
    child_process_id: Option<u32>,
    sequence: u64,
    reason: &'static str,
    snapshot: RingSnapshot,
) -> std::io::Result<()> {
    // Serialize automatic writers through creation and pruning. This makes the
    // configured count/byte limits true after every completed automatic dump,
    // even when several panes fire together.
    let _prune_guard = AUTOMATIC_DUMP_PRUNE_LOCK
        .lock()
        .unwrap_or_else(|value| value.into_inner());
    std::fs::create_dir_all(&config.dump_root)?;
    let directory = config.dump_root.join(format!(
        "auto-{}-{}-{pane_id}-{sequence}",
        std::process::id(),
        config.session_token,
    ));
    std::fs::create_dir(&directory)?;
    if let Err(error) = std::fs::write(directory.join(AUTOMATIC_DUMP_MARKER), b"1\n")
        .and_then(|_| {
            write_snapshot(
                &directory,
                config,
                pane_id,
                child_process_id,
                snapshot,
                Some(reason),
            )
            .map(|_| ())
        })
        .and_then(|_| std::fs::write(directory.join(AUTOMATIC_DUMP_COMPLETE), b"1\n"))
    {
        let _ = std::fs::remove_dir_all(&directory);
        return Err(error);
    }
    prune_automatic_dumps(config);
    Ok(())
}

fn prune_automatic_dumps(config: &Config) {
    let Ok(entries) = std::fs::read_dir(&config.dump_root) else {
        return;
    };
    let mut dumps = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join(AUTOMATIC_DUMP_MARKER).is_file() {
            continue;
        }
        let Some(key) = parse_automatic_dump_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let bytes = directory_bytes(&path);
        dumps.push((key, path, bytes));
    }
    dumps.sort_by_key(|entry| entry.0);
    let mut total: u64 = dumps.iter().map(|entry| entry.2).sum();
    let mut count = dumps.len();
    for (_, path, bytes) in dumps {
        if count <= config.retained_automatic_dumps
            && total <= config.automatic_dump_total_bytes as u64
        {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            count = count.saturating_sub(1);
            total = total.saturating_sub(bytes);
        }
    }
}

fn parse_automatic_dump_name(name: &str) -> Option<(u128, u64, u32, usize)> {
    let mut fields = name.strip_prefix("auto-")?.split('-');
    let pid = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    let pane = fields.next()?.parse().ok()?;
    let sequence = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((session, sequence, pid, pane))
}

#[cfg(not(test))]
fn prune_old_sessions(config: &Config) {
    let root = &config.dump_root;
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some((pid, _)) = parse_session_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        let bytes = directory_bytes(&path);
        sessions.push((modified, path, pid, bytes));
    }
    sessions.sort_by_key(|entry| entry.0);
    let mut total: u64 = sessions.iter().map(|entry| entry.3).sum();
    let mut count = sessions.len();
    for (_, path, pid, bytes) in sessions {
        if count <= config.retained_sessions && total <= config.total_disk_bytes as u64 {
            break;
        }
        if pid == std::process::id() || process_is_live(pid) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            count = count.saturating_sub(1);
            total = total.saturating_sub(bytes);
        }
    }
}

fn parse_session_name(name: &str) -> Option<(u32, u128)> {
    let rest = name.strip_prefix("session-")?;
    let (pid, token) = rest.split_once('-')?;
    if token.contains('-') {
        return None;
    }
    Some((pid.parse().ok()?, token.parse().ok()?))
}

fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                directory_bytes(&path)
            } else {
                entry.metadata().map_or(0, |metadata| metadata.len())
            }
        })
        .sum()
}

#[cfg(all(not(test), unix))]
fn process_is_live(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) performs no signal delivery and only probes the pid.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(all(not(test), windows))]
fn process_is_live(pid: u32) -> bool {
    type Handle = *mut std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    // SAFETY: OpenProcess/CloseHandle are called with a numeric pid and the
    // returned non-null handle is closed exactly once.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            false
        } else {
            let _ = CloseHandle(handle);
            true
        }
    }
}

#[cfg(test)]
pub(crate) fn with_test_config<T>(config: Option<Config>, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<Config>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_CONFIG.with(|value| *value.borrow_mut() = self.0.take());
        }
    }
    let next = config.map(Arc::new);
    let _restore = Restore(TEST_CONFIG.with(|value| value.replace(next)));
    action()
}

#[cfg(test)]
pub(crate) fn test_config(name: &str, origin: Instant) -> Config {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    let dump_root = std::env::temp_dir().join(format!(
        "renga-capture-{}-{name}-{}",
        std::process::id(),
        token
    ));
    let directory = dump_root.join(format!("session-{}-{token}", std::process::id()));
    Config {
        dump_root,
        continuous_directory: Some(directory.clone()),
        directory,
        origin,
        origin_unix_ms: 1000,
        ring_bytes: 4 * 1024 * 1024,
        file_bytes: 16 * 1024 * 1024,
        file_segments: 4,
        retained_sessions: 4,
        total_disk_bytes: 1024 * 1024 * 1024,
        retained_automatic_dumps: 32,
        automatic_dump_total_bytes: 128 * 1024 * 1024,
        dump_write_delay: Duration::ZERO,
        session_token: token as u128,
    }
}

#[cfg(test)]
pub(crate) struct TestCaptureCleanup(PathBuf);

#[cfg(test)]
impl TestCaptureCleanup {
    pub(crate) fn new(config: &Config) -> Self {
        assert_eq!(
            config.dump_root.parent(),
            Some(std::env::temp_dir().as_path())
        );
        assert!(config
            .dump_root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("renga-capture-"));
        Self(config.dump_root.clone())
    }
}

#[cfg(test)]
impl Drop for TestCaptureCleanup {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_reports_dec2026_deferred_kind() {
        let mut config = test_config("draw-dec2026-kind", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        let capture = Capture::create(config, 7, None, 3, 8).unwrap();
        capture.set_deferred(DeferredState {
            kind: DEFERRED_KIND_DEC2026,
            opened_at_elapsed_us: Some(17),
            buffered_len: 15,
        });
        capture.draw(true, 0);

        let snapshot = capture
            .snapshot_until(Instant::now() + Duration::from_secs(1))
            .expect("snapshot");
        let deferred = snapshot
            .records
            .iter()
            .find_map(|record| match &record.data {
                Data::AppDraw { deferred, .. } => Some(deferred),
                _ => None,
            });
        assert_eq!(
            deferred,
            Some(&DeferredState {
                kind: DEFERRED_KIND_DEC2026,
                opened_at_elapsed_us: Some(17),
                buffered_len: 15,
            })
        );
    }

    #[test]
    fn draw_reports_erase_hold_deferred_kind_control() {
        let mut config = test_config("draw-erase-kind", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        let capture = Capture::create(config, 7, None, 3, 8).unwrap();
        capture.set_deferred(DeferredState {
            kind: DEFERRED_KIND_ERASE_HOLD,
            opened_at_elapsed_us: Some(23),
            buffered_len: 11,
        });
        capture.draw(true, 0);

        let snapshot = capture
            .snapshot_until(Instant::now() + Duration::from_secs(1))
            .expect("snapshot");
        let deferred = snapshot
            .records
            .iter()
            .find_map(|record| match &record.data {
                Data::AppDraw { deferred, .. } => Some(deferred),
                _ => None,
            });
        assert_eq!(
            deferred,
            Some(&DeferredState {
                kind: DEFERRED_KIND_ERASE_HOLD,
                opened_at_elapsed_us: Some(23),
                buffered_len: 11,
            })
        );
    }

    #[test]
    fn ring_zero_disables_capture_without_files() {
        let mut config = test_config("disabled", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        config.ring_bytes = 0;
        config.continuous_directory = None;
        assert!(Capture::create(config.clone(), 1, None, 3, 8).is_none());
        assert!(!config.directory.exists());
    }

    #[test]
    fn always_on_ring_does_not_write_until_requested() {
        let origin = Instant::now();
        let mut config = test_config("ring-only", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        let capture = Capture::create(config.clone(), 3, None, 3, 8).unwrap();

        capture.read(origin, 0, b"hello", DeferredState::default());
        capture.applied(5);
        capture.draw(true, 0);
        capture.flush();
        std::thread::sleep(Duration::from_millis(800));
        assert!(!config.dump_root.exists());

        let report = dump_captures(vec![capture]).unwrap();
        assert_eq!(report.panes[0].status, "ok");
        assert!(Path::new(report.panes[0].bin_path.as_ref().unwrap()).is_file());
        assert!(Path::new(report.panes[0].jsonl_path.as_ref().unwrap()).is_file());
    }

    #[test]
    fn producer_backlog_stays_capped_and_records_a_gap() {
        let origin = Instant::now();
        let mut config = test_config("producer-backlog", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        config.ring_bytes = 1024;
        let capture = Capture::create(config.clone(), 4, None, 3, 8).unwrap();
        let (entered, entered_receive) = mpsc::channel();
        let (release, release_receive) = mpsc::channel();
        capture
            .sender
            .send(RecorderCommand::Block {
                entered,
                release: release_receive,
            })
            .unwrap();
        entered_receive
            .recv_timeout(Duration::from_secs(1))
            .expect("recorder blocked");

        let bytes = vec![b'x'; MAX_PTY_READ_BYTES];
        for index in 0..100_u64 {
            capture.read(
                origin + Duration::from_micros(index),
                index * MAX_PTY_READ_BYTES as u64,
                &bytes,
                DeferredState::default(),
            );
        }
        let backlog_cap = config.ring_bytes.max(MIN_AUXILIARY_BYTES);
        assert!(capture.queued_bytes.load(Ordering::Acquire) <= backlog_cap);
        assert!(capture.dropped_records.load(Ordering::Acquire) > 0);

        release.send(()).unwrap();
        let snapshot = capture
            .snapshot_until(Instant::now() + Duration::from_secs(1))
            .expect("snapshot after recorder release");
        assert!(snapshot.gaps > 0);
    }

    #[test]
    fn dump_writer_stall_returns_timed_out_by_deadline() {
        let origin = Instant::now();
        let mut config = test_config("writer-deadline", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        config.dump_write_delay = DUMP_DEADLINE + Duration::from_secs(1);
        let capture = Capture::create(config, 5, None, 3, 8).unwrap();
        capture.read(origin, 0, b"hello", DeferredState::default());

        let started = Instant::now();
        let report = dump_captures(vec![capture]).unwrap();
        assert!(started.elapsed() <= DUMP_DEADLINE + Duration::from_millis(500));
        assert_eq!(report.panes[0].status, "timed_out");
        assert_eq!(
            report.panes[0].reason.as_deref(),
            Some("dump deadline exceeded")
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read_dir(&report.directory)
                .unwrap()
                .flatten()
                .count(),
            0
        );
    }

    #[test]
    fn dump_is_replay_compatible_and_zero_based() {
        let mut config = test_config("dump", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        let capture = Capture::create(config, 7, Some(123), 3, 8).unwrap();
        capture.read(Instant::now(), 0, b"hello", DeferredState::default());
        capture.applied(5);
        capture.draw(true, 0);
        let report = dump_captures(vec![capture]).unwrap();
        let pane = &report.panes[0];
        assert_eq!(pane.status, "ok");
        replay::replay_file(Path::new(pane.jsonl_path.as_ref().unwrap()), 40_000).unwrap();
    }

    #[test]
    fn exact_session_name_parser_rejects_foreign_names() {
        assert_eq!(parse_session_name("session-12-34"), Some((12, 34)));
        assert_eq!(parse_session_name("session-12-34-extra"), None);
        assert_eq!(parse_session_name("other-12-34"), None);
    }

    #[test]
    fn automatic_signal_requires_erase_without_rewrite_payload() {
        assert!(erase_hold_closed_before_rewrite(b"\x1b[2J\x1b[H\x1b[?25l"));
        assert!(!erase_hold_closed_before_rewrite(b"\x1b[2J\x1b[Hrewritten"));
        assert!(!erase_hold_closed_before_rewrite(b"ordinary output"));
    }

    #[test]
    fn automatic_rate_limit_is_monotonic_and_per_pane() {
        let interval = AUTOMATIC_DUMP_INTERVAL.as_micros() as u64;
        let mut pane_a = AutomaticDumpLimiter::default();
        let mut pane_b = AutomaticDumpLimiter::default();
        assert!(pane_a.admit(1_000));
        assert!(!pane_a.admit(1_000 + interval - 1));
        assert!(pane_b.admit(1_000));
        assert!(pane_a.admit(1_000 + interval));
    }

    #[test]
    fn automatic_pruning_ignores_manual_and_unmarked_directories() {
        let mut config = test_config("auto-prune", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        config.retained_automatic_dumps = 1;
        config.automatic_dump_total_bytes = usize::MAX;
        std::fs::create_dir_all(&config.dump_root).unwrap();

        let old = config.dump_root.join("auto-10-20-30-0");
        let new = config.dump_root.join("auto-10-20-30-1");
        let manual = config.dump_root.join("manual-10-20");
        let unmarked = config.dump_root.join("auto-10-20-30-2");
        for path in [&old, &new, &manual, &unmarked] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(old.join(AUTOMATIC_DUMP_MARKER), b"1").unwrap();
        std::fs::write(new.join(AUTOMATIC_DUMP_MARKER), b"1").unwrap();

        prune_automatic_dumps(&config);

        assert!(!old.exists());
        assert!(new.exists());
        assert!(manual.exists());
        assert!(unmarked.exists());
    }

    #[test]
    fn forced_cut_schedules_once_then_records_rate_limit_suppression() {
        let mut config = test_config("forced-auto", Instant::now());
        config.continuous_directory = None;
        config.ring_bytes = 1;
        let _cleanup = TestCaptureCleanup::new(&config);
        let config = Arc::new(config);
        let mut ring = RingState::new(8, 80, config.ring_bytes);
        let first_len = ring.hard_cap;
        let record = |offset: u64, bytes: Vec<u8>, elapsed_us: u64| StoredRecord {
            timestamp_unix_ms: 1000,
            elapsed_us,
            data: Data::Read {
                bin_offset: offset,
                read_len: bytes.len(),
                deferred: DeferredState {
                    kind: "erase_hold",
                    opened_at_elapsed_us: Some(0),
                    buffered_len: bytes.len(),
                },
            },
            cost: bytes.len() + std::mem::size_of::<StoredRecord>() + 64,
            bytes: Some(bytes),
        };
        assert!(!ring.push(record(0, vec![b'x'; first_len], 0), 0));
        assert!(ring.push(record(first_len as u64, vec![b'y'], 1), 0));
        assert_eq!(ring.forced_cuts, 1);

        let mut limiter = AutomaticDumpLimiter::default();
        let mut sequence = 0;
        schedule_automatic_dump(
            config.clone(),
            77,
            None,
            &mut ring,
            &mut limiter,
            &mut sequence,
            1,
            "forced_cut",
        );
        schedule_automatic_dump(
            config.clone(),
            77,
            None,
            &mut ring,
            &mut limiter,
            &mut sequence,
            2,
            "forced_cut",
        );
        assert_eq!(ring.automatic_dump_suppressions, 1);

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let count = std::fs::read_dir(&config.dump_root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| {
                    entry.file_name().to_string_lossy().starts_with("auto-")
                        && entry.path().join(AUTOMATIC_DUMP_COMPLETE).is_file()
                })
                .count();
            if count == 1 {
                return;
            }
            std::thread::yield_now();
        }
        panic!("automatic forced-cut dump did not finish");
    }
}
