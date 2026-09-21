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
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[cfg(test)]
pub(crate) mod replay;

const HOLD_SLACK_BYTES: usize = 1024 * 1024;
const MAX_PTY_READ_BYTES: usize = 4096;
const MIN_AUXILIARY_BYTES: usize = 64 * 1024;
const DUMP_DEADLINE: Duration = Duration::from_secs(3);
const DUMP_REPLY_DEADLINE: Duration = Duration::from_secs(4);
const WRITER_QUEUE_RECORDS: usize = 256;
const AUTOMATIC_DUMP_INTERVAL: Duration = Duration::from_secs(10 * 60);
const AUTOMATIC_DUMP_DELAY: Duration = Duration::from_secs(10);

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
    pub(crate) automatic_dump_delay: Duration,
    dump_reply_deadline: Duration,
    automatic_dump_now: Arc<dyn Fn() -> Instant + Send + Sync>,
    #[cfg(test)]
    dump_write_gate: Option<Arc<TestWriteGate>>,
    #[cfg(test)]
    pub(crate) automatic_dump_done: Option<mpsc::Sender<()>>,
    session_token: u128,
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG: std::cell::RefCell<Option<Arc<Config>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
#[derive(Default)]
struct TestWriteGate {
    state: Mutex<TestWriteGateState>,
    signal: Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct TestWriteGateState {
    entered: bool,
    released: bool,
}

fn now_since_epoch() -> Option<Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
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
        automatic_dump_delay: AUTOMATIC_DUMP_DELAY,
        dump_reply_deadline: DUMP_REPLY_DEADLINE,
        automatic_dump_now: Arc::new(Instant::now),
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
    Metadata(Box<MetadataData>),
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
        #[serde(skip_serializing_if = "is_one")]
        repeat: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_elapsed_us: Option<u64>,
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
    Gap {
        bin_offset: u64,
        missing_raw_bytes: u64,
        dropped_records: u64,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MetadataData {
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
    record_budget_evictions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gaps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    automatic_dump_suppressions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    automatic_dump_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    automatic_trigger_elapsed_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_in_window: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    achieved_delay_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    partial: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_records: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_bytes: Option<usize>,
}

fn is_one(value: &u64) -> bool {
    *value == 1
}

fn data_offset(data: &Data) -> u64 {
    match data {
        Data::Read { bin_offset, .. }
        | Data::Transition { bin_offset, .. }
        | Data::ParserApply { bin_offset, .. }
        | Data::Gap { bin_offset, .. } => *bin_offset,
        Data::AppDraw { applied_offset, .. } | Data::Resize { applied_offset, .. } => {
            *applied_offset
        }
        Data::Metadata(..) | Data::AppTickRelease { .. } | Data::ReaderExit { .. } => 0,
    }
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
    queue_cost: usize,
}

#[derive(Clone, Copy)]
struct CompactRecord {
    elapsed_us: u64,
    a: u64,
    b: u64,
    x: u32,
    y: u32,
    z: u32,
    small: u16,
    kind: u8,
    flags: u8,
}

impl CompactRecord {
    fn from_stored(record: &StoredRecord) -> Self {
        let mut compact = Self {
            elapsed_us: record.elapsed_us,
            a: 0,
            b: 0,
            x: 0,
            y: 0,
            z: 0,
            small: 0,
            kind: 0,
            flags: 0,
        };
        let deferred = |state: &DeferredState| {
            let kind = match state.kind {
                DEFERRED_KIND_DEC2026 => 1,
                DEFERRED_KIND_ERASE_HOLD => 2,
                _ => 0,
            };
            (
                state.opened_at_elapsed_us.unwrap_or(u64::MAX),
                state.buffered_len.min(u32::MAX as usize) as u32,
                kind,
            )
        };
        match &record.data {
            Data::Metadata(_) => {}
            Data::Read {
                bin_offset,
                read_len,
                deferred: state,
            } => {
                let (opened, buffered, kind) = deferred(state);
                compact.kind = 1;
                compact.a = *bin_offset;
                compact.b = opened;
                compact.x = (*read_len).min(u32::MAX as usize) as u32;
                compact.y = buffered;
                compact.flags = kind;
            }
            Data::Transition {
                action,
                kind,
                marker,
                bin_offset,
                reason,
            } => {
                compact.kind = 2;
                compact.a = *bin_offset;
                compact.flags = match *action {
                    "open" => 1,
                    "close" => 2,
                    "promote" => 3,
                    _ => 0,
                } | (match *kind {
                    DEFERRED_KIND_DEC2026 => 1,
                    DEFERRED_KIND_ERASE_HOLD => 2,
                    _ => 0,
                } << 3);
                compact.x = match marker {
                    Some("dec2026_begin") => 1,
                    Some("dec2026_end") => 2,
                    Some("erase_display") => 3,
                    Some("erase_scrollback") => 4,
                    _ => 0,
                };
                compact.y = match reason {
                    Some("end_marker") => 1,
                    Some("timeout") => 2,
                    Some("byte_cap") => 3,
                    Some("reader_exit") => 4,
                    Some("tick_release") => 5,
                    Some("erase_conversion") => 6,
                    Some("inside_erase_hold") => 7,
                    Some("idle_timeout") => 8,
                    Some("max_duration") => 9,
                    other => {
                        debug_assert!(
                            other.is_none(),
                            "transition reason must have a compact encoding: {other:?}"
                        );
                        0
                    }
                };
            }
            Data::ParserApply {
                bin_offset,
                byte_len,
                applied_offset,
            } => {
                compact.kind = 3;
                compact.a = *bin_offset;
                compact.b = *applied_offset;
                compact.x = (*byte_len).min(u32::MAX as usize) as u32;
            }
            Data::AppTickRelease {
                released_len,
                deferred: state,
            } => {
                let (opened, buffered, kind) = deferred(state);
                compact.kind = 4;
                compact.a = opened;
                compact.x = (*released_len).min(u32::MAX as usize) as u32;
                compact.y = buffered;
                compact.flags = kind;
            }
            Data::AppDraw {
                drawn,
                applied_offset,
                scrollback,
                deferred: state,
                repeat,
                last_elapsed_us,
            } => {
                let (opened, buffered, deferred_kind) = deferred(state);
                compact.kind = 5;
                compact.a = *applied_offset;
                compact.b = opened;
                compact.x = (*scrollback).min(u32::MAX as usize) as u32;
                compact.y = buffered;
                compact.z = last_elapsed_us
                    .unwrap_or(record.elapsed_us)
                    .saturating_sub(record.elapsed_us)
                    .min(u32::MAX as u64) as u32;
                compact.small = (*repeat).min(u16::MAX as u64) as u16;
                compact.flags = deferred_kind | (u8::from(*drawn) << 2);
            }
            Data::Resize {
                rows,
                cols,
                clear,
                applied_offset,
            } => {
                compact.kind = 6;
                compact.a = *applied_offset;
                compact.x = u32::from(*rows);
                compact.y = u32::from(*cols);
                compact.flags = u8::from(*clear);
            }
            Data::ReaderExit { deferred: state } => {
                let (opened, buffered, kind) = deferred(state);
                compact.kind = 7;
                compact.a = opened;
                compact.x = buffered;
                compact.flags = kind;
            }
            Data::Gap {
                bin_offset,
                missing_raw_bytes,
                dropped_records,
            } => {
                compact.kind = 8;
                compact.a = *bin_offset;
                compact.b = *missing_raw_bytes;
                compact.x = (*dropped_records).min(u32::MAX as u64) as u32;
            }
        }
        compact
    }

    fn deferred(opened: u64, buffered: u32, kind: u8) -> DeferredState {
        DeferredState {
            kind: match kind & 3 {
                1 => DEFERRED_KIND_DEC2026,
                2 => DEFERRED_KIND_ERASE_HOLD,
                _ => DEFERRED_KIND_NONE,
            },
            opened_at_elapsed_us: (opened != u64::MAX).then_some(opened),
            buffered_len: buffered as usize,
        }
    }

    fn data(self) -> Data {
        match self.kind {
            1 => Data::Read {
                bin_offset: self.a,
                read_len: self.x as usize,
                deferred: Self::deferred(self.b, self.y, self.flags),
            },
            2 => Data::Transition {
                action: match self.flags & 7 {
                    1 => "open",
                    2 => "close",
                    3 => "promote",
                    _ => "marker",
                },
                kind: match (self.flags >> 3) & 3 {
                    1 => DEFERRED_KIND_DEC2026,
                    2 => DEFERRED_KIND_ERASE_HOLD,
                    _ => "marker",
                },
                marker: match self.x {
                    1 => Some("dec2026_begin"),
                    2 => Some("dec2026_end"),
                    3 => Some("erase_display"),
                    4 => Some("erase_scrollback"),
                    _ => None,
                },
                bin_offset: self.a,
                reason: match self.y {
                    1 => Some("end_marker"),
                    2 => Some("timeout"),
                    3 => Some("byte_cap"),
                    4 => Some("reader_exit"),
                    5 => Some("tick_release"),
                    6 => Some("erase_conversion"),
                    7 => Some("inside_erase_hold"),
                    8 => Some("idle_timeout"),
                    9 => Some("max_duration"),
                    code => {
                        debug_assert_eq!(code, 0, "unknown compact transition reason code");
                        None
                    }
                },
            },
            3 => Data::ParserApply {
                bin_offset: self.a,
                byte_len: self.x as usize,
                applied_offset: self.b,
            },
            4 => Data::AppTickRelease {
                released_len: self.x as usize,
                deferred: Self::deferred(self.a, self.y, self.flags),
            },
            5 => Data::AppDraw {
                drawn: self.flags & 4 != 0,
                applied_offset: self.a,
                scrollback: self.x as usize,
                deferred: Self::deferred(self.b, self.y, self.flags),
                repeat: u64::from(self.small),
                last_elapsed_us: (self.z > 0)
                    .then_some(self.elapsed_us.saturating_add(u64::from(self.z))),
            },
            6 => Data::Resize {
                rows: self.x as u16,
                cols: self.y as u16,
                clear: self.flags != 0,
                applied_offset: self.a,
            },
            7 => Data::ReaderExit {
                deferred: Self::deferred(self.a, self.x, self.flags),
            },
            8 => Data::Gap {
                bin_offset: self.a,
                missing_raw_bytes: self.b,
                dropped_records: u64::from(self.x),
            },
            _ => Data::Gap {
                bin_offset: 0,
                missing_raw_bytes: 0,
                dropped_records: 1,
            },
        }
    }
}

#[derive(Default)]
struct DropSummary {
    records: u64,
    raw_bytes: u64,
    first_offset: Option<u64>,
}

impl DropSummary {
    fn add(&mut self, record: &StoredRecord) {
        self.add_data(&record.data);
    }

    fn add_data(&mut self, data: &Data) {
        match data {
            Data::Gap {
                bin_offset,
                missing_raw_bytes,
                dropped_records,
            } => {
                self.records = self.records.saturating_add(*dropped_records);
                self.raw_bytes = self.raw_bytes.saturating_add(*missing_raw_bytes);
                self.first_offset = Some(
                    self.first_offset
                        .map_or(*bin_offset, |value| value.min(*bin_offset)),
                );
            }
            Data::Read {
                bin_offset,
                read_len,
                ..
            } => {
                self.records = self.records.saturating_add(1);
                self.raw_bytes = self.raw_bytes.saturating_add(*read_len as u64);
                self.first_offset = Some(
                    self.first_offset
                        .map_or(*bin_offset, |value| value.min(*bin_offset)),
                );
            }
            _ => self.records = self.records.saturating_add(1),
        }
    }

    fn take_record(&mut self, template: &StoredRecord) -> Option<StoredRecord> {
        self.take_record_for(
            template.timestamp_unix_ms,
            template.elapsed_us,
            &template.data,
        )
    }

    fn take_record_for(
        &mut self,
        timestamp_unix_ms: u128,
        elapsed_us: u64,
        data: &Data,
    ) -> Option<StoredRecord> {
        if self.records == 0 {
            return None;
        }
        let record = StoredRecord {
            timestamp_unix_ms,
            elapsed_us,
            data: Data::Gap {
                bin_offset: self.first_offset.unwrap_or_else(|| data_offset(data)),
                missing_raw_bytes: self.raw_bytes,
                dropped_records: self.records,
            },
            bytes: None,
            queue_cost: 0,
        };
        *self = Self::default();
        Some(record)
    }
}

// Boxing the record variant would add an allocation to every captured event.
// The channel stores commands out of line already, so retain the hot-path
// representation even though the administrative variants are much smaller.
#[allow(clippy::large_enum_variant)]
enum RecorderCommand {
    Record {
        gap: Option<StoredRecord>,
        record: StoredRecord,
    },
    AutomaticDump {
        triggered_at: Instant,
        due: Instant,
        elapsed_us: u64,
        reason: &'static str,
    },
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
    groups: Vec<Arc<RingGroup>>,
    raw: Arc<Vec<u8>>,
    evicted_raw_bytes: u64,
    evicted_records: u64,
    forced_cuts: u64,
    record_budget_evictions: u64,
    gaps: u64,
    automatic_dump_suppressions: u64,
    retained_raw_bytes: u64,
    queued_records: u64,
    queued_bytes: usize,
}

fn forward_disk_record(
    sender: &mpsc::SyncSender<DiskCommand>,
    record: &StoredRecord,
    drops: &mut DropSummary,
) {
    if let Some(gap) = drops.take_record(record) {
        let retry = gap.clone();
        if sender.try_send(DiskCommand::Record(gap)).is_err() {
            drops.add(&retry);
            drops.add(record);
            return;
        }
    }
    if sender
        .try_send(DiskCommand::Record(record.clone()))
        .is_err()
    {
        drops.add(record);
    }
}

impl RingSnapshot {
    fn records(&self) -> impl Iterator<Item = &CompactRecord> {
        self.groups.iter().flat_map(|group| group.records.iter())
    }

    fn record_count(&self) -> usize {
        self.groups.iter().map(|group| group.records.len()).sum()
    }

    fn suffix_for_deadline(
        mut self,
        deadline: Instant,
        now: &impl Fn() -> Instant,
    ) -> (Self, bool) {
        if self.groups.len() <= 2 {
            return (self, now() >= deadline);
        }
        let mut keep = 0_usize;
        for group in self.groups.iter().rev() {
            for record in &group.records {
                let _ = serde_json::to_writer(std::io::sink(), &record.data());
            }
            keep += 1;
            if keep >= 2 && now() >= deadline {
                break;
            }
        }
        if keep == self.groups.len() {
            return (self, false);
        }
        let split = self.groups.len() - keep;
        let omitted_raw: usize = self.groups[..split]
            .iter()
            .map(|group| group.raw_bytes)
            .sum();
        let omitted_records: u64 = self.groups[..split]
            .iter()
            .map(|group| group.records.len() as u64)
            .sum();
        self.evicted_raw_bytes = self.evicted_raw_bytes.saturating_add(omitted_raw as u64);
        self.evicted_records = self.evicted_records.saturating_add(omitted_records);
        self.groups.drain(..split);
        self.base = self.groups[0].start;
        self.raw = Arc::new(self.raw[omitted_raw..].to_vec());
        self.retained_raw_bytes = self.raw.len() as u64;
        (self, true)
    }
}

#[derive(Clone)]
struct RingGroup {
    start: u64,
    raw_bytes: usize,
    record_cost: usize,
    safe_start: bool,
    records: Vec<CompactRecord>,
}

struct RingState {
    rows: u16,
    cols: u16,
    base: u64,
    raw_end: u64,
    raw: VecDeque<u8>,
    prefix: Vec<CompactRecord>,
    groups: VecDeque<Arc<RingGroup>>,
    current: Option<RingGroup>,
    record_cost: usize,
    raw_bytes: usize,
    cap: usize,
    record_cap: usize,
    hard_cap: usize,
    evicted_raw_bytes: u64,
    evicted_records: u64,
    forced_cuts: u64,
    record_budget_evictions: u64,
    gaps: u64,
    automatic_dump_suppressions: u64,
    automatic_pin_start: Option<u64>,
    automatic_pin_pressure: bool,
    latest_applied: u64,
    prior_deferred: &'static str,
}

impl RingState {
    fn new(rows: u16, cols: u16, cap: usize) -> Self {
        let hard_cap = cap
            .saturating_add(HOLD_SLACK_BYTES)
            .saturating_add(MAX_PTY_READ_BYTES);
        Self {
            rows,
            cols,
            base: 0,
            raw_end: 0,
            raw: VecDeque::with_capacity(hard_cap),
            prefix: Vec::new(),
            groups: VecDeque::new(),
            current: None,
            record_cost: 0,
            raw_bytes: 0,
            cap,
            record_cap: cap.max(MIN_AUXILIARY_BYTES),
            hard_cap,
            evicted_raw_bytes: 0,
            evicted_records: 0,
            forced_cuts: 0,
            record_budget_evictions: 0,
            gaps: 0,
            automatic_dump_suppressions: 0,
            automatic_pin_start: None,
            automatic_pin_pressure: false,
            latest_applied: 0,
            prior_deferred: DEFERRED_KIND_NONE,
        }
    }

    fn push(&mut self, record: StoredRecord) -> bool {
        let forced_before = self.forced_cuts;
        let is_read = matches!(record.data, Data::Read { .. });
        let compact = CompactRecord::from_stored(&record);
        if let Data::Read {
            bin_offset,
            read_len,
            ref deferred,
        } = &record.data
        {
            if let Some(mut group) = self.current.take() {
                let old_cost = group.record_cost;
                group.records.shrink_to_fit();
                group.record_cost = compact_group_cost(&group.records);
                self.record_cost = self
                    .record_cost
                    .saturating_sub(old_cost)
                    .saturating_add(group.record_cost);
                self.groups.push_back(Arc::new(group));
            }
            let safe_start = *bin_offset > self.base
                && self.prior_deferred == DEFERRED_KIND_NONE
                && self.latest_applied >= *bin_offset;
            self.prior_deferred = deferred.kind;
            self.raw_end = bin_offset.saturating_add(*read_len as u64);
            self.raw_bytes = self.raw_bytes.saturating_add(*read_len);
            if let Some(bytes) = record.bytes.as_deref() {
                self.raw.extend(bytes);
            }
            let mut records = std::mem::take(&mut self.prefix);
            let old_cost = compact_group_cost(&records);
            records.push(compact);
            let record_cost = compact_group_cost(&records);
            self.record_cost = self
                .record_cost
                .saturating_sub(old_cost)
                .saturating_add(record_cost);
            self.current = Some(RingGroup {
                start: *bin_offset,
                raw_bytes: *read_len,
                record_cost,
                safe_start,
                records,
            });
        } else {
            if let Data::ParserApply { applied_offset, .. } = &record.data {
                self.latest_applied = *applied_offset;
            }
            if let Data::Gap {
                dropped_records, ..
            } = &record.data
            {
                self.gaps = self.gaps.saturating_add(*dropped_records);
            }
            if let Some(group) = &mut self.current {
                let old_cost = compact_group_cost(&group.records);
                group.records.push(compact);
                group.record_cost = compact_group_cost(&group.records);
                self.record_cost = self
                    .record_cost
                    .saturating_sub(old_cost)
                    .saturating_add(group.record_cost);
            } else {
                let old_cost = compact_group_cost(&self.prefix);
                self.prefix.push(compact);
                self.record_cost = self
                    .record_cost
                    .saturating_sub(old_cost)
                    .saturating_add(compact_group_cost(&self.prefix));
            }
        }
        self.evict_if_needed();
        if !is_read && self.record_cost > self.record_cap && self.groups.is_empty() {
            if self.automatic_pin_start.is_some() {
                self.automatic_pin_pressure = true;
            } else {
                self.drop_latest_structural_record();
            }
        }
        self.forced_cuts != forced_before
    }

    fn drop_latest_structural_record(&mut self) {
        let (dropped_cost, gap_cost) = if let Some(group) = &mut self.current {
            let old_cost = compact_group_cost(&group.records);
            let Some(dropped) = group.records.pop() else {
                return;
            };
            record_ring_drop(&mut group.records, dropped);
            group.records.shrink_to_fit();
            group.record_cost = compact_group_cost(&group.records);
            (old_cost, group.record_cost)
        } else {
            let old_cost = compact_group_cost(&self.prefix);
            let Some(dropped) = self.prefix.pop() else {
                return;
            };
            record_ring_drop(&mut self.prefix, dropped);
            self.prefix.shrink_to_fit();
            (old_cost, compact_group_cost(&self.prefix))
        };
        self.record_cost = self
            .record_cost
            .saturating_sub(dropped_cost)
            .saturating_add(gap_cost);
        self.evicted_records = self.evicted_records.saturating_add(1);
        self.gaps = self.gaps.saturating_add(1);
    }

    fn evict_if_needed(&mut self) {
        loop {
            let over_raw = self.raw_bytes > self.cap;
            let over_records = self.record_cost > self.record_cap;
            let over_hard = self.raw_bytes > self.hard_cap;
            if !over_raw && !over_records {
                break;
            }
            let next = self
                .groups
                .get(1)
                .map(Arc::as_ref)
                .or(self.current.as_ref());
            let Some(next) = next else {
                break;
            };
            let must_evict = over_records || over_hard;
            if over_raw && !must_evict && !next.safe_start {
                break;
            }
            let next_start = next.start;
            if self.automatic_pin_start.is_some_and(|pin_start| {
                self.groups
                    .front()
                    .is_none_or(|group| group.start >= pin_start)
            }) {
                self.automatic_pin_pressure = true;
                break;
            }
            let Some(removed) = self.groups.pop_front() else {
                break;
            };
            for record in &removed.records {
                if let Data::Resize { rows, cols, .. } = record.data() {
                    self.rows = rows;
                    self.cols = cols;
                }
            }
            self.raw_bytes = self.raw_bytes.saturating_sub(removed.raw_bytes);
            self.raw.drain(..removed.raw_bytes);
            self.record_cost = self.record_cost.saturating_sub(removed.record_cost);
            self.evicted_raw_bytes = self
                .evicted_raw_bytes
                .saturating_add(removed.raw_bytes as u64);
            self.evicted_records = self
                .evicted_records
                .saturating_add(removed.records.len() as u64);
            self.base = next_start;
            if over_records {
                self.record_budget_evictions = self.record_budget_evictions.saturating_add(1);
            }
            if over_hard {
                self.forced_cuts = self.forced_cuts.saturating_add(1);
            }
        }
    }

    fn pin_automatic_trigger(&mut self) {
        if self.automatic_pin_start.is_some() {
            return;
        }
        self.automatic_pin_start = self
            .current
            .as_ref()
            .map(|group| group.start)
            .or_else(|| self.groups.back().map(|group| group.start))
            .or(Some(self.base));
        self.automatic_pin_pressure = false;
    }

    fn take_automatic_pin_pressure(&mut self) -> bool {
        std::mem::take(&mut self.automatic_pin_pressure)
    }

    fn release_automatic_pin(&mut self) {
        self.automatic_pin_start = None;
        self.automatic_pin_pressure = false;
        self.evict_if_needed();
    }

    fn snapshot(&self, queued_records: u64, queued_bytes: usize) -> RingSnapshot {
        let mut groups: Vec<_> = self.groups.iter().cloned().collect();
        if let Some(current) = &self.current {
            groups.push(Arc::new(current.clone()));
        } else if !self.prefix.is_empty() {
            groups.push(Arc::new(RingGroup {
                start: self.base,
                raw_bytes: 0,
                record_cost: compact_group_cost(&self.prefix),
                safe_start: true,
                records: self.prefix.clone(),
            }));
        }
        RingSnapshot {
            rows: self.rows,
            cols: self.cols,
            base: self.base,
            groups,
            raw: Arc::new(self.raw.iter().copied().collect()),
            evicted_raw_bytes: self.evicted_raw_bytes,
            evicted_records: self.evicted_records,
            forced_cuts: self.forced_cuts,
            record_budget_evictions: self.record_budget_evictions,
            gaps: self.gaps,
            automatic_dump_suppressions: self.automatic_dump_suppressions,
            retained_raw_bytes: self.raw_bytes as u64,
            queued_records,
            queued_bytes,
        }
    }
}

fn compact_group_cost(records: &Vec<CompactRecord>) -> usize {
    if records.is_empty() {
        0
    } else {
        records.capacity() * std::mem::size_of::<CompactRecord>()
            + std::mem::size_of::<RingGroup>()
            + 2 * std::mem::size_of::<usize>()
    }
}

fn record_ring_drop(records: &mut Vec<CompactRecord>, dropped: CompactRecord) {
    if let Some(last) = records.last_mut() {
        if last.kind == 8 {
            last.x = last.x.saturating_add(1);
            return;
        }
    }
    let gap = CompactRecord {
        elapsed_us: dropped.elapsed_us,
        a: data_offset(&dropped.data()),
        b: 0,
        x: 1,
        y: 0,
        z: 0,
        small: 0,
        kind: 8,
        flags: 0,
    };
    records.push(gap);
}

#[derive(Default)]
struct AutomaticDumpLimiter {
    last_fired_elapsed_us: Option<u64>,
}

#[derive(Clone, Copy)]
struct PendingAutomaticDump {
    triggered_at: Instant,
    due: Instant,
    trigger_elapsed_us: u64,
    reason: &'static str,
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
    queued_records: Arc<AtomicU64>,
    enqueued_records: Arc<AtomicU64>,
    processed_records: Arc<AtomicU64>,
    drain_signal: Arc<(Mutex<()>, Condvar)>,
    dropped: Mutex<DropSummary>,
    ring: Arc<Mutex<RingState>>,
    recorder_alive: Arc<AtomicBool>,
    applied_offset: AtomicU64,
    deferred_kind: AtomicUsize,
    deferred_opened: AtomicU64,
    deferred_buffered: AtomicUsize,
    drawn_this_frame: AtomicBool,
    pending_draw: Mutex<Option<PendingDraw>>,
    dump_in_flight: AtomicBool,
}

struct PendingDraw {
    at: Instant,
    drawn: bool,
    applied_offset: u64,
    scrollback: usize,
    deferred: DeferredState,
    repeat: u64,
    last_elapsed_us: Option<u64>,
}

impl PendingDraw {
    fn same_state(
        &self,
        drawn: bool,
        applied_offset: u64,
        scrollback: usize,
        deferred: &DeferredState,
    ) -> bool {
        self.drawn == drawn
            && self.applied_offset == applied_offset
            && self.scrollback == scrollback
            && self.deferred == *deferred
    }

    fn into_data(self) -> (Instant, Data) {
        (
            self.at,
            Data::AppDraw {
                drawn: self.drawn,
                applied_offset: self.applied_offset,
                scrollback: self.scrollback,
                deferred: self.deferred,
                repeat: self.repeat,
                last_elapsed_us: self.last_elapsed_us,
            },
        )
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.flush_pending_draw();
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
        let queued_records = Arc::new(AtomicU64::new(0));
        let enqueued_records = Arc::new(AtomicU64::new(0));
        let processed_records = Arc::new(AtomicU64::new(0));
        let drain_signal = Arc::new((Mutex::new(()), Condvar::new()));
        let ring = Arc::new(Mutex::new(RingState::new(rows, cols, config.ring_bytes)));
        let recorder_alive = Arc::new(AtomicBool::new(true));
        let worker_queued = queued_bytes.clone();
        let worker_queued_records = queued_records.clone();
        let worker_processed_records = processed_records.clone();
        let worker_drain_signal = drain_signal.clone();
        let worker_ring = ring.clone();
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
                    worker_queued_records,
                    worker_processed_records,
                    worker_drain_signal,
                    worker_ring,
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
            queued_records,
            enqueued_records,
            processed_records,
            drain_signal,
            dropped: Mutex::new(DropSummary::default()),
            ring,
            recorder_alive,
            applied_offset: AtomicU64::new(0),
            deferred_kind: AtomicUsize::new(0),
            deferred_opened: AtomicU64::new(u64::MAX),
            deferred_buffered: AtomicUsize::new(0),
            drawn_this_frame: AtomicBool::new(false),
            pending_draw: Mutex::new(None),
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

    fn enqueue_inner(&self, at: Instant, data: Data, bytes: Option<Vec<u8>>) {
        if !self.is_enabled() {
            return;
        }
        let payload_bytes = bytes.as_ref().map_or(0, Vec::len);
        let queue_cost = payload_bytes + std::mem::size_of::<StoredRecord>() + 64;
        let backlog_cap = self.config.ring_bytes.max(MIN_AUXILIARY_BYTES);
        let charged =
            self.queued_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current
                        .checked_add(queue_cost)
                        .filter(|next| *next <= backlog_cap)
                });
        if charged.is_err() {
            self.dropped
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .add_data(&data);
            return;
        }
        let elapsed_us = self.elapsed_us(at);
        let timestamp_unix_ms = self.config.origin_unix_ms + u128::from(elapsed_us / 1000);
        let gap = self
            .dropped
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .take_record_for(timestamp_unix_ms, elapsed_us, &data);
        let record = StoredRecord {
            timestamp_unix_ms,
            elapsed_us,
            data,
            bytes,
            queue_cost,
        };
        let queued_record_count = 1 + u64::from(gap.is_some());
        self.queued_records
            .fetch_add(queued_record_count, Ordering::AcqRel);
        self.enqueued_records
            .fetch_add(queued_record_count, Ordering::AcqRel);
        if self
            .sender
            .send(RecorderCommand::Record { gap, record })
            .is_err()
        {
            self.queued_bytes.fetch_sub(queue_cost, Ordering::AcqRel);
            self.queued_records
                .fetch_sub(queued_record_count, Ordering::AcqRel);
            self.processed_records
                .fetch_add(queued_record_count, Ordering::AcqRel);
            self.drain_signal.1.notify_all();
        }
    }

    fn flush_pending_draw(&self) {
        let pending = self
            .pending_draw
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .take();
        if let Some(pending) = pending {
            let (at, data) = pending.into_data();
            self.enqueue_inner(at, data, None);
        }
    }

    fn enqueue(&self, at: Instant, data: Data, bytes: Option<Vec<u8>>) {
        self.flush_pending_draw();
        self.enqueue_inner(at, data, bytes);
    }

    pub(crate) fn event(&self, at: Instant, data: Data, _flush: bool) {
        self.enqueue(at, data, None);
    }

    pub(crate) fn automatic_dump(&self, at: Instant, reason: &'static str) {
        if !self.is_enabled() {
            return;
        }
        self.flush_pending_draw();
        let _ = self.sender.send(RecorderCommand::AutomaticDump {
            triggered_at: at,
            due: at + self.config.automatic_dump_delay,
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
        let at = Instant::now();
        let applied_offset = self.applied_offset.load(Ordering::Acquire);
        let deferred = self.deferred();
        let mut slot = self
            .pending_draw
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        if let Some(pending) = slot.as_mut() {
            if pending.same_state(drawn, applied_offset, scrollback, &deferred)
                && pending.repeat < u64::from(u16::MAX)
            {
                pending.repeat = pending.repeat.saturating_add(1);
                pending.last_elapsed_us = Some(self.elapsed_us(at));
                return;
            }
        }
        if let Some(pending) = slot.take() {
            let (pending_at, data) = pending.into_data();
            self.enqueue_inner(pending_at, data, None);
        }
        *slot = Some(PendingDraw {
            at,
            drawn,
            applied_offset,
            scrollback,
            deferred,
            repeat: 1,
            last_elapsed_us: None,
        });
    }

    pub(crate) fn finish_draw(&self) {
        if !self.drawn_this_frame.swap(false, Ordering::Relaxed) {
            self.draw(false, 0);
        }
    }

    fn flush_until(&self, deadline: Instant) {
        self.flush_pending_draw();
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
        self.flush_pending_draw();
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
        let watermark = self.enqueued_records.load(Ordering::Acquire);
        let wait_deadline = deadline.min(Instant::now() + Duration::from_millis(100));
        let (wait_lock, wait_signal) = &*self.drain_signal;
        let mut guard = wait_lock.lock().unwrap_or_else(|value| value.into_inner());
        while self.processed_records.load(Ordering::Acquire) < watermark {
            let remaining = wait_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let result = wait_signal
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|value| value.into_inner());
            guard = result.0;
        }
        drop(guard);
        let ring = self.ring.lock().unwrap_or_else(|value| value.into_inner());
        let mut snapshot = ring.snapshot(
            self.queued_records.load(Ordering::Acquire),
            self.queued_bytes.load(Ordering::Acquire),
        );
        snapshot.gaps = snapshot.gaps.saturating_add(
            self.dropped
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .records,
        );
        Ok(snapshot)
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
    queued_records: Arc<AtomicU64>,
    processed_records: Arc<AtomicU64>,
    drain_signal: Arc<(Mutex<()>, Condvar)>,
    ring: Arc<Mutex<RingState>>,
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
    let mut automatic_limiter = AutomaticDumpLimiter::default();
    let mut automatic_sequence = 0_u64;
    let mut disk_drops = DropSummary::default();
    let mut last_disk_record: Option<StoredRecord> = None;
    let mut pending_automatic: Option<PendingAutomaticDump> = None;
    loop {
        let command = if let Some(pending) = pending_automatic {
            match receiver.recv_timeout(
                pending
                    .due
                    .saturating_duration_since((config.automatic_dump_now)()),
            ) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let mut ring = ring.lock().unwrap_or_else(|value| value.into_inner());
                    let achieved_delay_us = duration_us(
                        (config.automatic_dump_now)()
                            .saturating_duration_since(pending.triggered_at),
                    );
                    write_automatic_from_ring(
                        config.clone(),
                        pane_id,
                        child_process_id,
                        &mut ring,
                        &mut automatic_sequence,
                        pending.trigger_elapsed_us,
                        pending.reason,
                        achieved_delay_us,
                    );
                    ring.release_automatic_pin();
                    pending_automatic = None;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            let Ok(command) = receiver.recv() else {
                break;
            };
            command
        };
        match command {
            RecorderCommand::Record { gap, record } => {
                let elapsed_us = record.elapsed_us;
                queued_bytes.fetch_sub(record.queue_cost, Ordering::AcqRel);
                queued_records.fetch_sub(1 + u64::from(gap.is_some()), Ordering::AcqRel);
                let processed = 1 + u64::from(gap.is_some());
                let mut ring = ring.lock().unwrap_or_else(|value| value.into_inner());
                if let Some(gap) = gap {
                    if let Some(sender) = &disk {
                        forward_disk_record(sender, &gap, &mut disk_drops);
                        last_disk_record = Some(gap.clone());
                    }
                    ring.push(gap);
                }
                if let Some(sender) = &disk {
                    forward_disk_record(sender, &record, &mut disk_drops);
                    last_disk_record = Some(record.clone());
                }
                let forced_cut = ring.push(record);
                if ring.take_automatic_pin_pressure() {
                    if let Some(pending) = pending_automatic.take() {
                        let achieved_delay_us = duration_us(
                            (config.automatic_dump_now)()
                                .saturating_duration_since(pending.triggered_at),
                        );
                        write_automatic_from_ring(
                            config.clone(),
                            pane_id,
                            child_process_id,
                            &mut ring,
                            &mut automatic_sequence,
                            pending.trigger_elapsed_us,
                            pending.reason,
                            achieved_delay_us,
                        );
                        ring.release_automatic_pin();
                    }
                }
                if forced_cut
                    && admit_automatic_dump(
                        config.clone(),
                        &mut ring,
                        &mut automatic_limiter,
                        elapsed_us,
                    )
                {
                    let triggered_at = config.origin + Duration::from_micros(elapsed_us);
                    ring.pin_automatic_trigger();
                    pending_automatic = Some(PendingAutomaticDump {
                        triggered_at,
                        due: triggered_at + config.automatic_dump_delay,
                        trigger_elapsed_us: elapsed_us,
                        reason: "forced_cut",
                    });
                }
                processed_records.fetch_add(processed, Ordering::AcqRel);
                drain_signal.1.notify_all();
            }
            RecorderCommand::AutomaticDump {
                triggered_at,
                due,
                elapsed_us,
                reason,
            } => {
                let mut ring = ring.lock().unwrap_or_else(|value| value.into_inner());
                if admit_automatic_dump(
                    config.clone(),
                    &mut ring,
                    &mut automatic_limiter,
                    elapsed_us,
                ) {
                    ring.pin_automatic_trigger();
                    pending_automatic = Some(PendingAutomaticDump {
                        triggered_at,
                        due,
                        trigger_elapsed_us: elapsed_us,
                        reason,
                    });
                }
            }
            RecorderCommand::Flush(done) => {
                if let Some(sender) = &disk {
                    if let Some(template) = last_disk_record.as_ref() {
                        if let Some(gap) = disk_drops.take_record(template) {
                            let _ = sender.send(DiskCommand::Record(gap));
                        }
                    }
                    let (disk_done, disk_receive) = mpsc::channel();
                    let _ = sender.send(DiskCommand::Flush(disk_done));
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
fn admit_automatic_dump(
    config: Arc<Config>,
    ring: &mut RingState,
    limiter: &mut AutomaticDumpLimiter,
    elapsed_us: u64,
) -> bool {
    if config.retained_automatic_dumps == 0
        || config.automatic_dump_total_bytes == 0
        || !limiter.admit(elapsed_us)
    {
        ring.automatic_dump_suppressions = ring.automatic_dump_suppressions.saturating_add(1);
        return false;
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn write_automatic_from_ring(
    config: Arc<Config>,
    pane_id: usize,
    child_process_id: Option<u32>,
    ring: &mut RingState,
    sequence: &mut u64,
    trigger_elapsed_us: u64,
    reason: &'static str,
    achieved_delay_us: u64,
) {
    let snapshot = ring.snapshot(0, 0);
    let first_elapsed_us = snapshot.records().next().map(|record| record.elapsed_us);
    let last_elapsed_us = snapshot.records().last().map(|record| record.elapsed_us);
    let trigger_in_window = first_elapsed_us.is_some_and(|first| {
        first <= trigger_elapsed_us
            && last_elapsed_us.is_some_and(|last| trigger_elapsed_us <= last)
    });
    let automatic = AutomaticSnapshotMetadata {
        reason,
        trigger_elapsed_us,
        trigger_in_window,
        achieved_delay_us,
    };
    let current_sequence = *sequence;
    *sequence = sequence.saturating_add(1);
    #[cfg(test)]
    let automatic_dump_done = config.automatic_dump_done.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("capture-auto-{pane_id}"))
        .spawn(move || {
            if write_automatic_snapshot(
                &config,
                pane_id,
                child_process_id,
                current_sequence,
                automatic,
                snapshot,
            )
            .is_err()
            {
                FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(test)]
            if let Some(done) = automatic_dump_done {
                let _ = done.send(());
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
    let metadata = Data::Metadata(Box::new(MetadataData {
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
        record_budget_evictions: Some(0),
        gaps: Some(0),
        automatic_dump_suppressions: Some(0),
        automatic_dump_reason: None,
        automatic_trigger_elapsed_us: None,
        trigger_in_window: None,
        achieved_delay_us: None,
        partial: None,
        queued_records: None,
        queued_bytes: None,
    }));
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
        Data::Metadata(..) => return None,
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
        Data::Gap {
            bin_offset,
            missing_raw_bytes,
            ..
        } => {
            let end = bin_offset.saturating_add(*missing_raw_bytes);
            if end <= base {
                return None;
            }
            if *bin_offset < base {
                *missing_raw_bytes = end - base;
                *bin_offset = 0;
            } else {
                *bin_offset -= base;
            }
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
    requester: &'static str,
    done: impl FnOnce(Result<DumpReport, String>) + Send + 'static,
) -> Result<(), String> {
    let spawn = std::thread::Builder::new()
        .name("pane-capture-dump".into())
        .spawn(move || done(dump_captures_for(captures, requester)));
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
    let writing = report
        .panes
        .iter()
        .filter(|pane| pane.status == "writing")
        .count();
    let failed = report.panes.len().saturating_sub(ok + writing);
    if writing > 0 {
        format!(
            "saved {ok} pane(s), {writing} still writing, {failed} failed; directory {}",
            report.directory
        )
    } else if failed == 0 {
        format!("saved {ok} pane(s) to {}", report.directory)
    } else {
        format!(
            "saved {ok} pane(s), {failed} failed; directory {}",
            report.directory
        )
    }
}

#[cfg(test)]
pub(crate) fn dump_captures(captures: Vec<Arc<Capture>>) -> Result<DumpReport, String> {
    dump_captures_for(captures, "test")
}

fn dump_captures_for(
    captures: Vec<Arc<Capture>>,
    requester: &'static str,
) -> Result<DumpReport, String> {
    dump_captures_with_clock(captures, requester, Instant::now)
}

fn dump_captures_with_clock<F>(
    captures: Vec<Arc<Capture>>,
    requester: &'static str,
    now: F,
) -> Result<DumpReport, String>
where
    F: Fn() -> Instant + Send + Sync + 'static,
{
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
    let selected_panes: Vec<_> = captures.iter().map(|capture| capture.pane_id).collect();
    let started = serde_json::json!({
        "state": "started",
        "started_at_unix_ms": now_since_epoch().unwrap_or_default().as_millis(),
        "requester": requester,
        "selector": { "pane_ids": &selected_panes },
        "panes": [],
    });
    if let Err(error) = std::fs::write(
        directory.join("outcome.json"),
        serde_json::to_vec_pretty(&started).unwrap_or_default(),
    ) {
        trace_dump_attempt("failed", requester, &directory, &selected_panes);
        let _ = std::fs::remove_dir(&directory);
        return Err(format!("cannot write dump outcome: {error}"));
    }
    trace_dump_attempt("started", requester, &directory, &selected_panes);
    let worker_count = captures.len();
    let reply_deadline = Instant::now() + first.config.dump_reply_deadline;
    let (done, receive) = mpsc::channel();
    let now = Arc::new(now);
    for (index, capture) in captures.into_iter().enumerate() {
        let done = done.clone();
        let worker_done = done.clone();
        let directory = directory.clone();
        let now = now.clone();
        let pane_id = capture.pane_id;
        let spawn = std::thread::Builder::new()
            .name(format!("capture-dump-pane-{pane_id}"))
            .spawn(move || {
                let deadline = now() + DUMP_DEADLINE;
                let report = match capture.snapshot_until(deadline) {
                    Ok(snapshot) => match write_snapshot_until(
                        directory,
                        capture.config.clone(),
                        pane_id,
                        capture.child_process_id,
                        snapshot,
                        deadline,
                        &|| now(),
                    ) {
                        Ok(report) => report,
                        Err(DeadlineWriteError::Failed(error)) => {
                            failed_dump(pane_id, "failed", &error)
                        }
                    },
                    Err(reason) => failed_dump(
                        pane_id,
                        if reason.contains("timed out") {
                            "timed_out"
                        } else {
                            "failed"
                        },
                        reason,
                    ),
                };
                let _ = worker_done.send((index, report));
            });
        if let Err(error) = spawn {
            let _ = done.send((index, failed_dump(pane_id, "failed", &error.to_string())));
        }
    }
    drop(done);
    let mut ordered: Vec<Option<PaneDumpReport>> = vec![None; worker_count];
    let mut remaining = worker_count;
    while remaining > 0 {
        let wait = reply_deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            break;
        }
        match receive.recv_timeout(wait) {
            Ok((index, report)) => {
                if ordered[index].replace(report).is_none() {
                    remaining -= 1;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                remaining = 0;
                break;
            }
        }
    }
    if remaining == 0 {
        let report = completed_dump_report(&directory, &selected_panes, ordered);
        finalize_manual_outcome(&directory, requester, &selected_panes, &report)?;
        return Ok(report);
    }

    let report = pending_dump_report(&directory, &selected_panes, &ordered);
    let writing = serde_json::json!({
        "state": "writing",
        "updated_at_unix_ms": now_since_epoch().unwrap_or_default().as_millis(),
        "requester": requester,
        "selector": { "pane_ids": &selected_panes },
        "panes": &report.panes,
    });
    std::fs::write(
        directory.join("outcome.json"),
        serde_json::to_vec_pretty(&writing).unwrap_or_default(),
    )
    .map_err(|error| format!("cannot update dump outcome: {error}"))?;
    trace_dump_attempt("writing", requester, &directory, &selected_panes);

    let final_directory = directory.clone();
    let final_selected_panes = selected_panes.clone();
    let spawn_failure_ordered = ordered.clone();
    let spawn = std::thread::Builder::new()
        .name("pane-capture-dump-finalizer".into())
        .spawn(move || {
            while remaining > 0 {
                let Ok((index, pane_report)) = receive.recv() else {
                    break;
                };
                if ordered[index].replace(pane_report).is_none() {
                    remaining -= 1;
                }
            }
            let final_report =
                completed_dump_report(&final_directory, &final_selected_panes, ordered);
            if finalize_manual_outcome(
                &final_directory,
                requester,
                &final_selected_panes,
                &final_report,
            )
            .is_err()
            {
                FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        });
    if spawn.is_err() {
        FAILURE_COUNT.fetch_add(1, Ordering::Relaxed);
        let failed_report =
            completed_dump_report(&directory, &selected_panes, spawn_failure_ordered);
        let _ = finalize_manual_outcome(&directory, requester, &selected_panes, &failed_report);
        return Err("cannot start pane capture dump finalizer".into());
    }
    Ok(report)
}

fn pending_dump_report(
    directory: &Path,
    selected_panes: &[usize],
    ordered: &[Option<PaneDumpReport>],
) -> DumpReport {
    DumpReport {
        directory: directory.to_string_lossy().into_owned(),
        panes: ordered
            .iter()
            .enumerate()
            .map(|(index, report)| {
                report.clone().unwrap_or_else(|| {
                    failed_dump(
                        selected_panes[index],
                        "writing",
                        "dump still writing; see outcome.json",
                    )
                })
            })
            .collect(),
    }
}

fn completed_dump_report(
    directory: &Path,
    selected_panes: &[usize],
    ordered: Vec<Option<PaneDumpReport>>,
) -> DumpReport {
    DumpReport {
        directory: directory.to_string_lossy().into_owned(),
        panes: ordered
            .into_iter()
            .enumerate()
            .map(|(index, report)| {
                report.unwrap_or_else(|| {
                    failed_dump(selected_panes[index], "failed", "dump writer stopped")
                })
            })
            .collect(),
    }
}

fn finalize_manual_outcome(
    directory: &Path,
    requester: &'static str,
    selected_panes: &[usize],
    report: &DumpReport,
) -> Result<(), String> {
    let final_state = if report.panes.iter().all(|pane| pane.status == "ok") {
        "written"
    } else if report
        .panes
        .iter()
        .any(|pane| pane.bin_path.is_some() || pane.jsonl_path.is_some())
    {
        "partial"
    } else {
        "failed"
    };
    let outcome = serde_json::json!({
        "state": final_state,
        "finished_at_unix_ms": now_since_epoch().unwrap_or_default().as_millis(),
        "requester": requester,
        "selector": { "pane_ids": &selected_panes },
        "panes": &report.panes,
    });
    if let Err(error) = std::fs::write(
        directory.join("outcome.json"),
        serde_json::to_vec_pretty(&outcome).unwrap_or_default(),
    ) {
        trace_dump_attempt("failed", requester, directory, selected_panes);
        return Err(format!("cannot finalize dump outcome: {error}"));
    }
    trace_dump_attempt(final_state, requester, directory, selected_panes);
    Ok(())
}

fn trace_dump_attempt(state: &str, requester: &str, directory: &Path, pane_ids: &[usize]) {
    let Some(path) = crate::app::codex_peer_debug_log_path() else {
        return;
    };
    crate::app::append_codex_peer_debug_record(
        &path,
        serde_json::json!({
            "action": "pane_capture_dump",
            "state": state,
            "requester": requester,
            "directory": directory,
            "pane_ids": pane_ids,
        }),
    );
}

enum DeadlineWriteError {
    Failed(String),
}

#[derive(Clone, Copy)]
struct AutomaticSnapshotMetadata {
    reason: &'static str,
    trigger_elapsed_us: u64,
    trigger_in_window: bool,
    achieved_delay_us: u64,
}

fn write_snapshot_until(
    directory: PathBuf,
    config: Arc<Config>,
    pane_id: usize,
    child_process_id: Option<u32>,
    snapshot: RingSnapshot,
    deadline: Instant,
    now: &impl Fn() -> Instant,
) -> Result<PaneDumpReport, DeadlineWriteError> {
    let (snapshot, partial) = snapshot.suffix_for_deadline(deadline, now);
    #[cfg(test)]
    if let Some(gate) = &config.dump_write_gate {
        let mut state = gate.state.lock().unwrap_or_else(|value| value.into_inner());
        state.entered = true;
        gate.signal.notify_all();
        while !state.released {
            state = gate
                .signal
                .wait(state)
                .unwrap_or_else(|value| value.into_inner());
        }
    }
    let staging_directory = directory.join(format!(".pane-{pane_id}-pending"));
    std::fs::create_dir(&staging_directory)
        .map_err(|error| DeadlineWriteError::Failed(error.to_string()))?;
    let result = write_snapshot(
        &staging_directory,
        &config,
        pane_id,
        child_process_id,
        snapshot,
        None,
        partial,
    );
    let mut report = match result {
        Ok(report) => report,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging_directory);
            return Err(DeadlineWriteError::Failed(error.to_string()));
        }
    };
    let staged_bin = PathBuf::from(report.bin_path.as_deref().unwrap_or_default());
    let staged_jsonl = PathBuf::from(report.jsonl_path.as_deref().unwrap_or_default());
    let bin_path = directory.join(format!("pane-{pane_id}.bin"));
    let jsonl_path = directory.join(format!("pane-{pane_id}.jsonl"));
    std::fs::rename(&staged_bin, &bin_path)
        .map_err(|error| DeadlineWriteError::Failed(error.to_string()))?;
    std::fs::rename(&staged_jsonl, &jsonl_path)
        .map_err(|error| DeadlineWriteError::Failed(error.to_string()))?;
    let _ = std::fs::remove_dir(&staging_directory);
    report.bin_path = Some(bin_path.to_string_lossy().into_owned());
    report.jsonl_path = Some(jsonl_path.to_string_lossy().into_owned());
    if partial {
        report.status = "timed_out";
        report.reason = Some("dump deadline exceeded; newest suffix written".to_owned());
    } else {
        let _ = std::fs::write(directory.join(format!("pane-{pane_id}.complete")), b"1\n");
    }
    Ok(report)
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
    automatic: Option<AutomaticSnapshotMetadata>,
    partial: bool,
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
    let first_elapsed = snapshot.records().next().map(|record| record.elapsed_us);
    let last_elapsed = snapshot.records().last().map(|record| record.elapsed_us);
    let retained_records = snapshot.record_count() as u64;
    let retained_raw = snapshot.retained_raw_bytes;
    let metadata = Data::Metadata(Box::new(MetadataData {
        version: 2,
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
        record_budget_evictions: Some(snapshot.record_budget_evictions),
        gaps: Some(snapshot.gaps),
        automatic_dump_suppressions: Some(snapshot.automatic_dump_suppressions),
        automatic_dump_reason: automatic.map(|metadata| metadata.reason),
        automatic_trigger_elapsed_us: automatic.map(|metadata| metadata.trigger_elapsed_us),
        trigger_in_window: automatic.map(|metadata| metadata.trigger_in_window),
        achieved_delay_us: automatic.map(|metadata| metadata.achieved_delay_us),
        partial: partial.then_some(true),
        queued_records: Some(snapshot.queued_records),
        queued_bytes: Some(snapshot.queued_bytes),
    }));
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
    let mut raw_cursor = 0_usize;
    for record in snapshot.records() {
        let original = record.data();
        let read_len = match &original {
            Data::Read { read_len, .. } => *read_len,
            _ => 0,
        };
        let Some(data) = rebase_data(original, snapshot.base) else {
            raw_cursor = raw_cursor.saturating_add(read_len);
            continue;
        };
        if read_len > 0 {
            let end = raw_cursor.saturating_add(read_len);
            if end > snapshot.raw.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ring raw bytes are shorter than read records",
                ));
            }
            binary.write_all(&snapshot.raw[raw_cursor..end])?;
            raw_cursor = end;
        }
        write_compact_record(&mut jsonl, record.elapsed_us, data)?;
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

fn write_compact_record(
    writer: &mut BufWriter<File>,
    elapsed_us: u64,
    data: Data,
) -> std::io::Result<()> {
    let value = match data {
        Data::Metadata(_) => return Ok(()),
        Data::Read {
            bin_offset,
            read_len,
            deferred,
        } => serde_json::json!([
            "r",
            elapsed_us,
            bin_offset,
            read_len,
            deferred.kind,
            deferred.opened_at_elapsed_us,
            deferred.buffered_len
        ]),
        Data::Transition {
            action,
            kind,
            marker,
            bin_offset,
            reason,
        } => serde_json::json!(["t", elapsed_us, action, kind, marker, bin_offset, reason]),
        Data::ParserApply {
            bin_offset,
            byte_len,
            applied_offset,
        } => serde_json::json!(["a", elapsed_us, bin_offset, byte_len, applied_offset]),
        Data::AppTickRelease {
            released_len,
            deferred,
        } => serde_json::json!([
            "k",
            elapsed_us,
            released_len,
            deferred.kind,
            deferred.opened_at_elapsed_us,
            deferred.buffered_len
        ]),
        Data::AppDraw {
            drawn,
            applied_offset,
            scrollback,
            deferred,
            repeat,
            last_elapsed_us,
        } => serde_json::json!([
            "d",
            elapsed_us,
            drawn,
            applied_offset,
            scrollback,
            deferred.kind,
            deferred.opened_at_elapsed_us,
            deferred.buffered_len,
            repeat,
            last_elapsed_us
        ]),
        Data::Resize {
            rows,
            cols,
            clear,
            applied_offset,
        } => serde_json::json!(["z", elapsed_us, rows, cols, clear, applied_offset]),
        Data::ReaderExit { deferred } => serde_json::json!([
            "x",
            elapsed_us,
            deferred.kind,
            deferred.opened_at_elapsed_us,
            deferred.buffered_len
        ]),
        Data::Gap {
            bin_offset,
            missing_raw_bytes,
            dropped_records,
        } => serde_json::json!([
            "g",
            elapsed_us,
            bin_offset,
            missing_raw_bytes,
            dropped_records
        ]),
    };
    serde_json::to_writer(&mut *writer, &value).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
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
    automatic: AutomaticSnapshotMetadata,
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
    let pane_ids = [pane_id];
    trace_dump_attempt("started", "automatic", &directory, &pane_ids);
    if let Err(error) = std::fs::write(
        directory.join("outcome.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "state": "started",
            "requester": "automatic",
            "trigger_reason": automatic.reason,
            "trigger_elapsed_us": automatic.trigger_elapsed_us,
            "trigger_in_window": automatic.trigger_in_window,
            "achieved_delay_us": automatic.achieved_delay_us,
            "pane_id": pane_id,
        }))?,
    ) {
        trace_dump_attempt("failed", "automatic", &directory, &pane_ids);
        return Err(error);
    }
    let mut pane_report = None;
    if let Err(error) = std::fs::write(directory.join(AUTOMATIC_DUMP_MARKER), b"1\n")
        .and_then(|_| {
            write_snapshot(
                &directory,
                config,
                pane_id,
                child_process_id,
                snapshot,
                Some(automatic),
                false,
            )
            .map(|report| pane_report = Some(report))
        })
        .and_then(|_| {
            std::fs::write(
                directory.join("outcome.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "state": "written",
                    "requester": "automatic",
                    "trigger_reason": automatic.reason,
                    "trigger_elapsed_us": automatic.trigger_elapsed_us,
                    "trigger_in_window": automatic.trigger_in_window,
                    "achieved_delay_us": automatic.achieved_delay_us,
                    "pane_id": pane_id,
                    "panes": pane_report.iter().collect::<Vec<_>>(),
                }))?,
            )
        })
        .and_then(|_| std::fs::write(directory.join(AUTOMATIC_DUMP_COMPLETE), b"1\n"))
    {
        let _ = std::fs::write(
            directory.join("outcome.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "state": "failed",
                "requester": "automatic",
                "trigger_reason": automatic.reason,
                "trigger_elapsed_us": automatic.trigger_elapsed_us,
                "trigger_in_window": automatic.trigger_in_window,
                "achieved_delay_us": automatic.achieved_delay_us,
                "pane_id": pane_id,
                "reason": error.to_string(),
            }))
            .unwrap_or_default(),
        );
        trace_dump_attempt("failed", "automatic", &directory, &pane_ids);
        return Err(error);
    }
    trace_dump_attempt("written", "automatic", &directory, &pane_ids);
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
        automatic_dump_delay: AUTOMATIC_DUMP_DELAY,
        dump_reply_deadline: DUMP_REPLY_DEADLINE,
        automatic_dump_now: Arc::new(Instant::now),
        dump_write_gate: None,
        automatic_dump_done: None,
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
        let deferred = snapshot.records().find_map(|record| match record.data() {
            Data::AppDraw { deferred, .. } => Some(deferred),
            _ => None,
        });
        assert_eq!(
            deferred,
            Some(DeferredState {
                kind: "dec2026",
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
        let deferred = snapshot.records().find_map(|record| match record.data() {
            Data::AppDraw { deferred, .. } => Some(deferred),
            _ => None,
        });
        assert_eq!(
            deferred,
            Some(DeferredState {
                kind: "erase_hold",
                opened_at_elapsed_us: Some(23),
                buffered_len: 11,
            })
        );
    }

    #[test]
    fn equal_draws_coalesce_before_ring_fanout() {
        let mut config = test_config("draw-coalesce", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        let capture = Capture::create(config, 8, None, 3, 8).unwrap();
        for _ in 0..10_000 {
            capture.draw(false, 0);
        }
        let snapshot = capture
            .snapshot_until(Instant::now() + Duration::from_secs(1))
            .expect("snapshot");
        let draws: Vec<_> = snapshot
            .records()
            .filter_map(|record| match record.data() {
                Data::AppDraw { repeat, .. } => Some(repeat),
                _ => None,
            })
            .collect();
        assert_eq!(draws, vec![10_000]);
    }

    #[test]
    fn structural_ring_stays_charged_when_one_group_fills() {
        assert!(std::mem::size_of::<CompactRecord>() <= 48);
        let mut ring = RingState::new(3, 8, 1024);
        let queue_cost = std::mem::size_of::<StoredRecord>() + 64;
        for elapsed_us in 0..10_000 {
            ring.push(StoredRecord {
                timestamp_unix_ms: 1000,
                elapsed_us,
                data: Data::AppTickRelease {
                    released_len: 0,
                    deferred: DeferredState::default(),
                },
                bytes: None,
                queue_cost,
            });
        }
        assert!(ring.record_cost <= ring.record_cap + std::mem::size_of::<CompactRecord>());
        assert!(ring.evicted_records > 0);
        assert!(matches!(
            ring.prefix.last().map(|record| record.data()),
            Some(Data::Gap { dropped_records, .. }) if dropped_records > 0
        ));
    }

    #[test]
    fn compact_transition_reasons_round_trip_without_loss() {
        for reason in [
            "end_marker",
            "timeout",
            "byte_cap",
            "reader_exit",
            "tick_release",
            "erase_conversion",
            "inside_erase_hold",
            "idle_timeout",
            "max_duration",
        ] {
            let record = StoredRecord {
                timestamp_unix_ms: 1_000,
                elapsed_us: 2_000,
                data: Data::Transition {
                    action: "close",
                    kind: DEFERRED_KIND_DEC2026,
                    marker: None,
                    bin_offset: 42,
                    reason: Some(reason),
                },
                bytes: None,
                queue_cost: 0,
            };

            match CompactRecord::from_stored(&record).data() {
                Data::Transition {
                    reason: compact_reason,
                    ..
                } => assert_eq!(compact_reason, Some(reason)),
                data => panic!("expected transition, got {data:?}"),
            }
        }
    }

    #[test]
    fn field_shape_record_eviction_is_not_an_automatic_dump_trigger() {
        let mut ring = RingState::new(40, 120, 4 * 1024 * 1024);
        let interval_us = 8_520_u64;
        let count = 128_000_u64;
        for index in 0..count {
            let bytes = vec![b'x'; 17];
            assert!(!ring.push(StoredRecord {
                timestamp_unix_ms: 1000,
                elapsed_us: index * interval_us,
                data: Data::Read {
                    bin_offset: index * 17,
                    read_len: bytes.len(),
                    deferred: DeferredState::default(),
                },
                bytes: Some(bytes),
                queue_cost: std::mem::size_of::<StoredRecord>() + 81,
            }));
        }
        let snapshot = ring.snapshot(0, 0);
        let retained_us = snapshot
            .records()
            .last()
            .unwrap()
            .elapsed_us
            .saturating_sub(snapshot.records().next().unwrap().elapsed_us);
        assert!(ring.record_budget_evictions > 0);
        assert_eq!(ring.forced_cuts, 0);
        assert!(retained_us >= 60_000_000, "retained only {retained_us} us");
    }

    #[test]
    #[ignore = "release-mode performance gate"]
    fn ring_push_cost_stays_constant_at_capacity() {
        fn push_reads(ring: &mut RingState, start: u64, count: u64) -> Duration {
            let began = Instant::now();
            for index in start..start + count {
                let bytes = vec![b'x'; 64];
                ring.push(StoredRecord {
                    timestamp_unix_ms: 1000,
                    elapsed_us: index,
                    data: Data::Read {
                        bin_offset: index * 64,
                        read_len: bytes.len(),
                        deferred: DeferredState::default(),
                    },
                    bytes: Some(bytes),
                    queue_cost: std::mem::size_of::<StoredRecord>() + 128,
                });
            }
            began.elapsed()
        }

        let mut ring = RingState::new(40, 120, 4 * 1024 * 1024);
        let _ = push_reads(&mut ring, 0, 100_000);
        let first = push_reads(&mut ring, 100_000, 50_000);
        let last = push_reads(&mut ring, 150_000, 50_000);
        let first_us = first.as_secs_f64() * 1_000_000.0 / 50_000.0;
        let last_us = last.as_secs_f64() * 1_000_000.0 / 50_000.0;
        eprintln!(
            "ring_push_us first={first_us:.3} last={last_us:.3} compact_record_bytes={} queue_record_bytes={}",
            std::mem::size_of::<CompactRecord>(),
            std::mem::size_of::<StoredRecord>(),
        );
        assert!(first_us <= 10.0, "first window {first_us:.3} us/push");
        assert!(last_us <= 10.0, "last window {last_us:.3} us/push");
        assert!(
            last_us <= first_us * 2.0,
            "last window {last_us:.3} exceeds 2x first {first_us:.3}"
        );
    }

    #[test]
    #[ignore = "field helper: set RENGA_PANE_REPLAY to pane-24.jsonl"]
    fn field_capture_retention_window() {
        let path =
            PathBuf::from(std::env::var_os("RENGA_PANE_REPLAY").expect("set RENGA_PANE_REPLAY"));
        let binary = std::fs::read(path.with_extension("bin")).unwrap();
        let jsonl = std::fs::read_to_string(path).unwrap();
        let mut binary_cursor = 0_usize;
        let mut ring = RingState::new(40, 120, 4 * 1024 * 1024);
        let deferred = |value: &serde_json::Value| DeferredState {
            kind: match value["kind"].as_str().unwrap_or("none") {
                "dec2026" => DEFERRED_KIND_DEC2026,
                "erase_hold" => DEFERRED_KIND_ERASE_HOLD,
                _ => DEFERRED_KIND_NONE,
            },
            opened_at_elapsed_us: value["opened_at_elapsed_us"].as_u64(),
            buffered_len: value["buffered_len"].as_u64().unwrap_or(0) as usize,
        };
        for line in jsonl.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let event = value["event"].as_str().unwrap();
            if event == "metadata" {
                continue;
            }
            let data = match event {
                "read" => Data::Read {
                    bin_offset: value["bin_offset"].as_u64().unwrap(),
                    read_len: value["read_len"].as_u64().unwrap() as usize,
                    deferred: deferred(&value["deferred"]),
                },
                "transition" => Data::Transition {
                    action: match value["action"].as_str().unwrap() {
                        "open" => "open",
                        "close" => "close",
                        "promote" => "promote",
                        _ => "marker",
                    },
                    kind: match value["kind"].as_str().unwrap() {
                        "dec2026" => DEFERRED_KIND_DEC2026,
                        "erase_hold" => DEFERRED_KIND_ERASE_HOLD,
                        _ => "marker",
                    },
                    marker: match value["marker"].as_str() {
                        Some("dec2026_begin") => Some("dec2026_begin"),
                        Some("dec2026_end") => Some("dec2026_end"),
                        Some("erase_display") => Some("erase_display"),
                        Some("erase_scrollback") => Some("erase_scrollback"),
                        _ => None,
                    },
                    bin_offset: value["bin_offset"].as_u64().unwrap(),
                    reason: match value["reason"].as_str() {
                        Some("end_marker") => Some("end_marker"),
                        Some("timeout") => Some("timeout"),
                        Some("byte_cap") => Some("byte_cap"),
                        Some("reader_exit") => Some("reader_exit"),
                        Some("tick_release") => Some("tick_release"),
                        Some("erase_conversion") => Some("erase_conversion"),
                        Some("inside_erase_hold") => Some("inside_erase_hold"),
                        Some("idle_timeout") => Some("idle_timeout"),
                        Some("max_duration") => Some("max_duration"),
                        _ => None,
                    },
                },
                "parser_apply" => Data::ParserApply {
                    bin_offset: value["bin_offset"].as_u64().unwrap(),
                    byte_len: value["byte_len"].as_u64().unwrap() as usize,
                    applied_offset: value["applied_offset"].as_u64().unwrap(),
                },
                "app_tick_release" => Data::AppTickRelease {
                    released_len: value["released_len"].as_u64().unwrap() as usize,
                    deferred: deferred(&value["deferred"]),
                },
                "app_draw" => Data::AppDraw {
                    drawn: value["drawn"].as_bool().unwrap(),
                    applied_offset: value["applied_offset"].as_u64().unwrap(),
                    scrollback: value["scrollback"].as_u64().unwrap() as usize,
                    deferred: deferred(&value["deferred"]),
                    repeat: value["repeat"].as_u64().unwrap_or(1),
                    last_elapsed_us: value["last_elapsed_us"].as_u64(),
                },
                "resize" => Data::Resize {
                    rows: value["rows"].as_u64().unwrap() as u16,
                    cols: value["cols"].as_u64().unwrap() as u16,
                    clear: value["clear"].as_bool().unwrap(),
                    applied_offset: value["applied_offset"].as_u64().unwrap(),
                },
                "reader_exit" => Data::ReaderExit {
                    deferred: deferred(&value["deferred"]),
                },
                "gap" => Data::Gap {
                    bin_offset: value["bin_offset"].as_u64().unwrap(),
                    missing_raw_bytes: value["missing_raw_bytes"].as_u64().unwrap(),
                    dropped_records: value["dropped_records"].as_u64().unwrap(),
                },
                other => panic!("unknown event {other}"),
            };
            let bytes = if let Data::Read { read_len, .. } = data {
                let end = binary_cursor + read_len;
                let bytes = binary[binary_cursor..end].to_vec();
                binary_cursor = end;
                Some(bytes)
            } else {
                None
            };
            ring.push(StoredRecord {
                timestamp_unix_ms: value["timestamp_unix_ms"].as_u64().unwrap_or(0) as u128,
                elapsed_us: value["elapsed_us"].as_u64().unwrap(),
                data,
                queue_cost: bytes.as_ref().map_or(0, Vec::len)
                    + std::mem::size_of::<StoredRecord>()
                    + 64,
                bytes,
            });
        }
        let snapshot = ring.snapshot(0, 0);
        let first = snapshot.records().next().unwrap().elapsed_us;
        let last = snapshot.records().last().unwrap().elapsed_us;
        eprintln!(
            "field_retention seconds={:.3} raw_bytes={} records={} compact_record_bytes={}",
            last.saturating_sub(first) as f64 / 1_000_000.0,
            snapshot.retained_raw_bytes,
            snapshot.record_count(),
            std::mem::size_of::<CompactRecord>()
        );
        assert!(last.saturating_sub(first) >= 60_000_000);
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
        assert!(
            capture
                .dropped
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .records
                > 0
        );

        release.send(()).unwrap();
        let snapshot = capture
            .snapshot_until(Instant::now() + Duration::from_secs(1))
            .expect("snapshot after recorder release");
        assert!(snapshot.gaps > 0);
    }

    #[test]
    fn dump_writer_stall_returns_timed_out_by_deadline() {
        assert_eq!(DUMP_DEADLINE, Duration::from_secs(3));
        assert!(DUMP_DEADLINE < crate::ipc::APP_REPLY_TIMEOUT);
        let origin = Instant::now();
        let mut config = test_config("writer-deadline", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        let capture = Capture::create(config, 5, None, 3, 8).unwrap();
        capture.read(origin, 0, b"hello", DeferredState::default());
        capture.read(origin, 5, b"world", DeferredState::default());
        capture.read(origin, 10, b"again", DeferredState::default());

        let clock_calls = Arc::new(AtomicUsize::new(0));
        let test_clock_calls = clock_calls.clone();
        let test_now = move || {
            if test_clock_calls.fetch_add(1, Ordering::Relaxed) < 1 {
                origin
            } else {
                origin + Duration::from_secs(4)
            }
        };
        let report = dump_captures_with_clock(vec![capture], "test", test_now).unwrap();
        assert_eq!(report.panes[0].status, "timed_out");
        assert_eq!(
            report.panes[0].reason.as_deref(),
            Some("dump deadline exceeded; newest suffix written")
        );
        let jsonl = Path::new(report.panes[0].jsonl_path.as_ref().unwrap());
        assert!(jsonl.is_file());
        replay::replay_file(jsonl, 40_000).unwrap();
        let reads: Vec<_> = std::fs::read_to_string(jsonl)
            .unwrap()
            .lines()
            .filter_map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                (value.as_array().and_then(|items| items[0].as_str()) == Some("r"))
                    .then(|| value[2].as_u64().unwrap())
            })
            .collect();
        assert_eq!(reads, vec![0, 5]);
        let outcome: serde_json::Value = serde_json::from_slice(
            &std::fs::read(Path::new(&report.directory).join("outcome.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(outcome["state"], "partial");
        assert_eq!(outcome["panes"][0]["status"], "timed_out");
    }

    #[test]
    fn stalled_writer_replies_writing_then_finalizes_outcome() {
        assert_eq!(DUMP_REPLY_DEADLINE, Duration::from_secs(4));
        assert!(DUMP_REPLY_DEADLINE < crate::ipc::APP_REPLY_TIMEOUT);
        let origin = Instant::now();
        let mut config = test_config("writer-reply-deadline", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        config.dump_reply_deadline = Duration::from_millis(50);
        let gate = Arc::new(TestWriteGate::default());
        config.dump_write_gate = Some(gate.clone());
        let capture = Capture::create(config, 51, None, 3, 8).unwrap();
        capture.read(origin, 0, b"hello", DeferredState::default());

        let started = Instant::now();
        let report = dump_captures(vec![capture]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(report.panes[0].status, "writing");
        assert!(format_dump_report(&report).contains("still writing"));
        assert!(format_dump_report(&report).contains(&report.directory));

        let interim: serde_json::Value = serde_json::from_slice(
            &std::fs::read(Path::new(&report.directory).join("outcome.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(interim["state"], "writing");
        {
            let mut state = gate.state.lock().unwrap_or_else(|value| value.into_inner());
            if !state.entered {
                let result = gate
                    .signal
                    .wait_timeout(state, Duration::from_secs(1))
                    .unwrap_or_else(|value| value.into_inner());
                state = result.0;
            }
            assert!(state.entered);
            state.released = true;
            gate.signal.notify_all();
        }

        let outcome_path = Path::new(&report.directory).join("outcome.json");
        let wait_until = Instant::now() + Duration::from_secs(2);
        let outcome = loop {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&outcome_path).unwrap()).unwrap();
            if value["state"] != "writing" {
                break value;
            }
            assert!(Instant::now() < wait_until, "outcome did not finalize");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(outcome["state"], "written");
        assert_eq!(outcome["panes"][0]["status"], "ok");
        assert!(Path::new(outcome["panes"][0]["bin_path"].as_str().unwrap()).is_file());
        assert!(Path::new(outcome["panes"][0]["jsonl_path"].as_str().unwrap()).is_file());
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
        let jsonl_path = Path::new(pane.jsonl_path.as_ref().unwrap());
        let lines: Vec<_> = std::fs::read_to_string(jsonl_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect();
        assert_eq!(lines[0]["version"], 2);
        assert!(lines[1].is_array());
        replay::replay_file(jsonl_path, 40_000).unwrap();
        let outcome: serde_json::Value = serde_json::from_slice(
            &std::fs::read(Path::new(&report.directory).join("outcome.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(outcome["requester"], "test");
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
        config.automatic_dump_delay = Duration::ZERO;
        let (automatic_dump_done, automatic_dump_receive) = mpsc::channel();
        config.automatic_dump_done = Some(automatic_dump_done);
        let cleanup = TestCaptureCleanup::new(&config);
        let dump_root = config.dump_root.clone();
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
            queue_cost: bytes.len() + std::mem::size_of::<StoredRecord>() + 64,
            bytes: Some(bytes),
        };
        assert!(!ring.push(record(0, vec![b'x'; first_len], 0)));
        assert!(ring.push(record(first_len as u64, vec![b'y'], 1)));
        assert_eq!(ring.forced_cuts, 1);

        let mut limiter = AutomaticDumpLimiter::default();
        let mut sequence = 0;
        assert!(admit_automatic_dump(
            config.clone(),
            &mut ring,
            &mut limiter,
            1,
        ));
        assert!(!admit_automatic_dump(
            config.clone(),
            &mut ring,
            &mut limiter,
            2,
        ));
        assert_eq!(ring.automatic_dump_suppressions, 1);
        write_automatic_from_ring(
            config.clone(),
            77,
            None,
            &mut ring,
            &mut sequence,
            1,
            "forced_cut",
            0,
        );

        automatic_dump_receive
            .recv_timeout(Duration::from_secs(2))
            .expect("automatic forced-cut writer completion");
        let count = std::fs::read_dir(&config.dump_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with("auto-")
                    && entry.path().join(AUTOMATIC_DUMP_COMPLETE).is_file()
            })
            .count();
        assert_eq!(count, 1);
        drop(config);
        drop(cleanup);
        assert!(!dump_root.exists());
    }

    #[test]
    fn automatic_dump_waits_after_trigger_without_restarting_delay() {
        let origin = Instant::now();
        let mut config = test_config("automatic-delay", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        config.automatic_dump_delay = Duration::from_millis(120);
        let (done, receive) = mpsc::channel();
        config.automatic_dump_done = Some(done);
        let dump_root = config.dump_root.clone();
        let capture = Capture::create(config, 91, None, 3, 8).unwrap();

        capture.read(origin, 0, b"zero", DeferredState::default());
        capture.automatic_dump(origin, "erase_hold_cap");
        std::thread::sleep(Duration::from_millis(40));
        capture.read(
            origin + Duration::from_secs(5),
            4,
            b"five",
            DeferredState::default(),
        );
        capture.automatic_dump(origin + Duration::from_secs(5), "forced_cut");
        std::thread::sleep(Duration::from_millis(50));
        capture.read(
            origin + Duration::from_secs(10),
            8,
            b"ten!",
            DeferredState::default(),
        );
        receive
            .recv_timeout(Duration::from_secs(2))
            .expect("automatic dump completion");
        capture.read(
            origin + Duration::from_secs(15),
            12,
            b"late",
            DeferredState::default(),
        );

        let directory = std::fs::read_dir(&dump_root)
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().starts_with("auto-"))
            .unwrap()
            .path();
        let outcome: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("outcome.json")).unwrap())
                .unwrap();
        assert_eq!(outcome["state"], "written");
        assert_eq!(outcome["trigger_elapsed_us"], 0);
        assert_eq!(outcome["panes"][0]["status"], "ok");
        let records: Vec<serde_json::Value> =
            std::fs::read_to_string(directory.join("pane-91.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        let offsets: Vec<_> = records
            .iter()
            .filter(|record| record.as_array().and_then(|items| items[0].as_str()) == Some("r"))
            .map(|record| record[2].as_u64().unwrap())
            .collect();
        assert_eq!(offsets, vec![0, 4, 8]);
    }

    #[test]
    fn automatic_dump_ends_delay_before_trigger_group_eviction() {
        let origin = Instant::now();
        let mut config = test_config("automatic-pin", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        config.ring_bytes = MIN_AUXILIARY_BYTES;
        config.automatic_dump_delay = Duration::from_secs(5);
        let (done, receive) = mpsc::channel();
        config.automatic_dump_done = Some(done);
        let dump_root = config.dump_root.clone();
        let capture = Capture::create(config, 92, None, 3, 8).unwrap();

        capture.read(origin, 0, b"t", DeferredState::default());
        capture.applied(1);
        capture.automatic_dump(origin, "erase_hold_cap");
        for index in 1..2_000_u64 {
            let at = origin + Duration::from_micros(index);
            capture.read(at, index, b"x", DeferredState::default());
            capture.applied(1);
            if index % 100 == 0 {
                capture.flush();
            }
        }
        capture.flush();
        receive
            .recv_timeout(Duration::from_secs(2))
            .expect("automatic dump should end its delay under ring pressure");

        let directory = std::fs::read_dir(&dump_root)
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().starts_with("auto-"))
            .unwrap()
            .path();
        let outcome: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("outcome.json")).unwrap())
                .unwrap();
        assert_eq!(outcome["state"], "written");
        assert_eq!(outcome["trigger_in_window"], true);
        assert!(outcome["achieved_delay_us"].as_u64().unwrap() < 5_000_000);

        let metadata: serde_json::Value = std::fs::read_to_string(directory.join("pane-92.jsonl"))
            .unwrap()
            .lines()
            .next()
            .map(|line| serde_json::from_str(line).unwrap())
            .unwrap();
        assert_eq!(metadata["trigger_in_window"], true);
        assert_eq!(metadata["automatic_trigger_elapsed_us"], 0);
        assert!(metadata["achieved_delay_us"].as_u64().unwrap() < 5_000_000);
    }

    #[test]
    fn automatic_dump_releases_eviction_pin_after_completion() {
        let origin = Instant::now();
        let mut config = test_config("automatic-pin-release", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        config.ring_bytes = MIN_AUXILIARY_BYTES;
        config.automatic_dump_delay = Duration::ZERO;
        let (done, receive) = mpsc::channel();
        config.automatic_dump_done = Some(done);
        let capture = Capture::create(config, 93, None, 3, 8).unwrap();
        let idle = DeferredState {
            kind: DEFERRED_KIND_NONE,
            ..DeferredState::default()
        };

        capture.read(origin, 0, b"trigger", idle.clone());
        capture.applied(7);
        capture.automatic_dump(origin, "erase_hold_cap");
        receive
            .recv_timeout(Duration::from_secs(2))
            .expect("automatic dump completion");

        let mut offset = 7_u64;
        for index in 0..256_u64 {
            let bytes = vec![index as u8; 1024];
            capture.read(
                origin + Duration::from_micros(index + 1),
                offset,
                &bytes,
                idle.clone(),
            );
            capture.applied(bytes.len());
            offset += bytes.len() as u64;
            if index % 16 == 15 {
                capture.flush();
            }
        }
        capture.flush();

        let ring = capture
            .ring
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        assert!(ring.automatic_pin_start.is_none());
        assert!(
            ring.raw_bytes <= ring.cap,
            "raw_bytes={} cap={} groups={} safe={} base={} latest_applied={} pin={:?}",
            ring.raw_bytes,
            ring.cap,
            ring.groups.len(),
            ring.groups.iter().filter(|group| group.safe_start).count(),
            ring.base,
            ring.latest_applied,
            ring.automatic_pin_start,
        );
        assert!(ring.raw.len() <= ring.hard_cap);
        assert!(ring.evicted_raw_bytes > 0);
    }

    #[test]
    fn raw_ring_eviction_drains_the_retained_byte_prefix() {
        let origin = Instant::now();
        let mut config = test_config("raw-eviction-bytes", origin);
        let _cleanup = TestCaptureCleanup::new(&config);
        config.continuous_directory = None;
        config.ring_bytes = 16;
        std::fs::create_dir_all(&config.dump_root).unwrap();

        let mut ring = RingState::new(3, 8, config.ring_bytes);
        let idle = DeferredState {
            kind: DEFERRED_KIND_NONE,
            ..DeferredState::default()
        };
        for index in 0..32_u64 {
            let bytes = vec![index as u8; 4];
            ring.push(StoredRecord {
                timestamp_unix_ms: 1000,
                elapsed_us: index * 2,
                data: Data::Read {
                    bin_offset: index * 4,
                    read_len: bytes.len(),
                    deferred: idle.clone(),
                },
                queue_cost: bytes.len() + std::mem::size_of::<StoredRecord>() + 64,
                bytes: Some(bytes),
            });
            ring.push(StoredRecord {
                timestamp_unix_ms: 1000,
                elapsed_us: index * 2 + 1,
                data: Data::ParserApply {
                    bin_offset: index * 4,
                    byte_len: 4,
                    applied_offset: index * 4 + 4,
                },
                bytes: None,
                queue_cost: std::mem::size_of::<StoredRecord>() + 64,
            });
        }

        let snapshot = ring.snapshot(0, 0);
        let expected: Vec<u8> = snapshot
            .records()
            .filter_map(|record| match record.data() {
                Data::Read {
                    bin_offset,
                    read_len,
                    ..
                } => Some(vec![(bin_offset / 4) as u8; read_len]),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(ring.raw_bytes <= ring.cap);
        assert!(ring.raw.len() <= ring.cap);
        assert_eq!(snapshot.raw.as_slice(), expected.as_slice());

        let report =
            write_snapshot(&config.dump_root, &config, 94, None, snapshot, None, false).unwrap();
        assert_eq!(std::fs::read(report.bin_path.unwrap()).unwrap(), expected,);
    }
}
