use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::codex_peer::{append_codex_peer_debug_record, codex_peer_debug_log_path};
use super::AppCommandTiming;

pub(crate) const FRAME_OVER_BUDGET_MS: u128 = 500;
pub(crate) const ACTION_FRAME_OVER_BUDGET: &str = "frame_over_budget";
pub(crate) const ACTION_IPC_COMMAND_PROCESSED: &str = "ipc_command_processed";
pub(crate) const PHASE_EVENT_DRAIN: &str = "event_drain";
pub(crate) const PHASE_IPC_COMMANDS: &str = "ipc_commands";
pub(crate) const PHASE_CODEX_FLUSH: &str = "codex_flush";
pub(crate) const PHASE_RENDER: &str = "render";
pub(crate) const PHASE_OTHER: &str = "other";
const FIELD_ACTION: &str = "action";
const FIELD_FRAME_MS: &str = "frame_ms";
const FIELD_PHASE_MS: &str = "phase_ms";
const FIELD_EVENTS_DRAINED: &str = "events_drained";
const FIELD_PTY_OUTPUT_EVENTS: &str = "pty_output_events";
const FIELD_IPC_COMMANDS: &str = "ipc_commands";
const FIELD_PTY_WRITES: &str = "pty_writes";
const FIELD_OUTPUT_BYTES_BY_PANE: &str = "output_bytes_by_pane";
const FIELD_LOCK_WAIT_MS_BY_PANE: &str = "lock_wait_ms_by_pane";
const FIELD_VISIBLE_PANES: &str = "visible_panes";
const FIELD_COMMAND: &str = "command";
const FIELD_PANE_ID: &str = "pane_id";
const FIELD_ENQUEUED_AT_MS: &str = "enqueued_at_ms";
const FIELD_QUEUE_WAIT_MS: &str = "queue_wait_ms";
const FIELD_COMMAND_MS: &str = "command_ms";

#[derive(Default)]
struct FrameData {
    started_at: Option<Instant>,
    phase_ms: BTreeMap<&'static str, u128>,
    events_drained: usize,
    pty_output_events: usize,
    ipc_commands: Vec<IpcCommandMetric>,
    pty_writes_by_pane: BTreeMap<usize, usize>,
    output_bytes_by_pane: BTreeMap<usize, usize>,
    lock_wait: BTreeMap<usize, Duration>,
}

struct IpcCommandMetric {
    kind: &'static str,
    pane_id: Option<usize>,
    command_ms: u128,
    queue_wait_ms: u128,
}

#[derive(Default)]
struct DiagnosticsState {
    path: Option<OsString>,
    frame: Option<FrameData>,
}

thread_local! {
    static STATE: RefCell<DiagnosticsState> = RefCell::new(DiagnosticsState::default());
}

pub(crate) fn configure_from_env() {
    configure(codex_peer_debug_log_path());
}

fn configure(path: Option<OsString>) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.path = path;
        state.frame = None;
    });
}

pub(crate) fn begin_frame(started_at: Instant) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.path.is_some() {
            state.frame = Some(FrameData {
                started_at: Some(started_at),
                ..FrameData::default()
            });
        }
    });
}

pub(crate) fn phase_started() -> Option<Instant> {
    STATE.with(|state| state.borrow().frame.as_ref().map(|_| Instant::now()))
}

pub(crate) fn finish_phase(phase: &'static str, started_at: Option<Instant>) {
    let Some(started_at) = started_at else {
        return;
    };
    let elapsed = started_at.elapsed().as_millis();
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            *frame.phase_ms.entry(phase).or_default() += elapsed;
        }
    });
}

pub(crate) fn lock_wait_started() -> Option<Instant> {
    phase_started()
}

pub(crate) fn record_lock_wait(pane_id: usize, started_at: Option<Instant>) {
    let Some(started_at) = started_at else {
        return;
    };
    let elapsed = started_at.elapsed();
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            *frame.lock_wait.entry(pane_id).or_default() += elapsed;
        }
    });
}

pub(crate) fn record_pty_write(pane_id: usize) {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            *frame.pty_writes_by_pane.entry(pane_id).or_default() += 1;
        }
    });
}

pub(crate) fn record_app_event(pty_output: Option<(usize, usize)>) {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            frame.events_drained += 1;
            if let Some((pane_id, bytes)) = pty_output {
                frame.pty_output_events += 1;
                *frame.output_bytes_by_pane.entry(pane_id).or_default() += bytes;
            }
        }
    });
}

pub(crate) fn record_ipc_command(
    kind: &'static str,
    pane_id: Option<usize>,
    timing: AppCommandTiming,
    handle_started_at: Instant,
) {
    let Some(enqueued_at) = timing.enqueued_at else {
        return;
    };
    let command_ms = handle_started_at.elapsed().as_millis();
    let queue_wait_ms = handle_started_at
        .checked_duration_since(enqueued_at)
        .unwrap_or_default()
        .as_millis();
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(path) = state.path.clone() else {
            return;
        };
        if let Some(frame) = state.frame.as_mut() {
            frame.ipc_commands.push(IpcCommandMetric {
                kind,
                pane_id,
                command_ms,
                queue_wait_ms,
            });
        }
        drop(state);
        append_codex_peer_debug_record(
            &path,
            json!({
                (FIELD_ACTION): ACTION_IPC_COMMAND_PROCESSED,
                (FIELD_COMMAND): kind,
                (FIELD_PANE_ID): pane_id,
                (FIELD_ENQUEUED_AT_MS): timing.enqueued_at_unix_ms,
                (FIELD_QUEUE_WAIT_MS): queue_wait_ms,
                (FIELD_COMMAND_MS): command_ms,
            }),
        );
    });
}

pub(crate) fn finish_frame(visible_panes: impl FnOnce() -> Vec<usize>) {
    if !STATE.with(|state| state.borrow().frame.is_some()) {
        return;
    }
    finish_frame_at(Instant::now(), visible_panes());
}

fn finish_frame_at(finished_at: Instant, visible_panes: Vec<usize>) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(path) = state.path.clone() else {
            return;
        };
        let Some(frame) = state.frame.take() else {
            return;
        };
        let frame_ms = finished_at
            .checked_duration_since(frame.started_at.expect("started frame"))
            .unwrap_or_default()
            .as_millis();
        if frame_ms <= FRAME_OVER_BUDGET_MS {
            return;
        }

        let measured_ms: u128 = frame.phase_ms.values().sum();
        let mut phase_ms = frame.phase_ms;
        phase_ms.insert(PHASE_OTHER, frame_ms.saturating_sub(measured_ms));
        for phase in [
            PHASE_EVENT_DRAIN,
            PHASE_IPC_COMMANDS,
            PHASE_CODEX_FLUSH,
            PHASE_RENDER,
        ] {
            phase_ms.entry(phase).or_default();
        }

        let ipc_commands: Vec<Value> = frame
            .ipc_commands
            .into_iter()
            .map(|metric| {
                json!({
                    (FIELD_COMMAND): metric.kind,
                    (FIELD_PANE_ID): metric.pane_id,
                    (FIELD_QUEUE_WAIT_MS): metric.queue_wait_ms,
                    (FIELD_COMMAND_MS): metric.command_ms,
                })
            })
            .collect();
        let pty_writes: Vec<Value> = frame
            .pty_writes_by_pane
            .into_iter()
            .map(|(pane_id, count)| json!({ (FIELD_PANE_ID): pane_id, "count": count }))
            .collect();
        let output_bytes_by_pane: BTreeMap<String, usize> = frame
            .output_bytes_by_pane
            .into_iter()
            .map(|(pane_id, bytes)| (pane_id.to_string(), bytes))
            .collect();
        let lock_wait_ms_by_pane: BTreeMap<String, u128> = frame
            .lock_wait
            .into_iter()
            .map(|(pane_id, elapsed)| (pane_id.to_string(), elapsed.as_millis()))
            .collect();
        drop(state);
        append_codex_peer_debug_record(
            OsStr::new(&path),
            json!({
                (FIELD_ACTION): ACTION_FRAME_OVER_BUDGET,
                (FIELD_FRAME_MS): frame_ms,
                (FIELD_PHASE_MS): phase_ms,
                (FIELD_EVENTS_DRAINED): frame.events_drained,
                (FIELD_PTY_OUTPUT_EVENTS): frame.pty_output_events,
                (FIELD_IPC_COMMANDS): ipc_commands,
                (FIELD_PTY_WRITES): pty_writes,
                (FIELD_OUTPUT_BYTES_BY_PANE): output_bytes_by_pane,
                (FIELD_LOCK_WAIT_MS_BY_PANE): lock_wait_ms_by_pane,
                (FIELD_VISIBLE_PANES): visible_panes,
            }),
        );
    });
}

#[cfg(test)]
mod debug_logging_tests {
    use super::*;
    use crate::app::AppCommand;
    use crate::pane::Pane;

    fn temp_log_path(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "renga-frame-debug-{label}-{}-{unique}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn enabled_over_budget_frame_writes_one_jsonl_record() {
        let path = temp_log_path("enabled");
        configure(Some(path.as_os_str().to_owned()));
        let started_at = Instant::now();
        begin_frame(started_at);
        finish_frame_at(
            started_at + Duration::from_millis(FRAME_OVER_BUDGET_MS as u64 + 1),
            vec![3, 7],
        );
        let within_budget_started_at = Instant::now();
        begin_frame(within_budget_started_at);
        finish_frame_at(
            within_budget_started_at + Duration::from_millis(FRAME_OVER_BUDGET_MS as u64 - 1),
            vec![3, 7],
        );

        let contents = std::fs::read_to_string(&path).expect("debug JSONL");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: Value = serde_json::from_str(lines[0]).expect("one JSON object");
        assert_eq!(record["action"], ACTION_FRAME_OVER_BUDGET);
        assert_eq!(record["frame_ms"], (FRAME_OVER_BUDGET_MS + 1) as u64);
        assert_eq!(record["visible_panes"], json!([3, 7]));
        assert!(record["process_id"].is_number());
        assert!(record["record_sequence"].is_number());
        assert!(record["timestamp_unix_ms"].is_number());
        std::fs::remove_file(path).expect("remove debug JSONL");
        configure(None);
    }

    #[test]
    fn disabled_over_budget_frame_writes_no_record() {
        let path = temp_log_path("disabled");
        configure(None);
        let started_at = Instant::now();
        begin_frame(started_at);
        finish_frame_at(
            started_at + Duration::from_millis(FRAME_OVER_BUDGET_MS as u64 + 1),
            vec![1],
        );
        assert!(!path.exists());
    }

    #[test]
    fn timed_app_command_records_enqueue_queue_and_processing_times() {
        let path = temp_log_path("ipc");
        configure(Some(path.as_os_str().to_owned()));
        begin_frame(Instant::now());

        let mut app = crate::app::App::new(40, 80).expect("headless app");
        let (reply_tx, reply_rx) = oneshot::channel();
        crate::app::set_ipc_enqueue_timing_test_override(Some(true));
        let command = crate::app::with_ipc_enqueue_timing(AppCommand::List { reply: reply_tx });
        crate::app::set_ipc_enqueue_timing_test_override(Some(false));
        assert!(matches!(command, AppCommand::Timed { .. }));
        app.command_tx.send(command).expect("enqueue timed command");
        app.drain_app_commands();
        let _ = reply_rx.recv().expect("list response");

        let contents = std::fs::read_to_string(&path).expect("debug JSONL");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: Value = serde_json::from_str(lines[0]).expect("one JSON object");
        assert_eq!(record["action"], ACTION_IPC_COMMAND_PROCESSED);
        assert_eq!(record["command"], "list");
        assert!(record["pane_id"].is_null());
        assert!(record["enqueued_at_ms"].is_number());
        assert!(record["queue_wait_ms"].is_number());
        assert!(record["command_ms"].is_number());
        std::fs::remove_file(path).expect("remove debug JSONL");
        configure(None);
    }

    #[test]
    fn scrollbar_info_records_parser_lock_wait_for_its_pane() {
        let path = temp_log_path("scrollbar-lock");
        configure(Some(path.as_os_str().to_owned()));
        let started_at = Instant::now();
        begin_frame(started_at);

        let (event_tx, _event_rx) = std::sync::mpsc::channel();
        let pane = Pane::new(9991, 24, 80, event_tx).expect("headless pane");
        let parser = pane.parser.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = parser.lock().unwrap_or_else(|error| error.into_inner());
            locked_tx.send(()).expect("signal held parser lock");
            std::thread::sleep(Duration::from_millis(300));
        });
        locked_rx.recv().expect("wait for held parser lock");

        let _ = pane.scrollbar_info();
        holder.join().expect("lock holder exits");
        finish_frame_at(
            started_at + Duration::from_millis(FRAME_OVER_BUDGET_MS as u64 + 1),
            vec![pane.id],
        );

        let contents = std::fs::read_to_string(&path).expect("debug JSONL");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let record: Value = serde_json::from_str(lines[0]).expect("one JSON object");
        let wait_ms = record[FIELD_LOCK_WAIT_MS_BY_PANE][pane.id.to_string()]
            .as_u64()
            .expect("pane lock wait milliseconds");
        assert!(
            wait_ms >= 200,
            "scrollbar lock wait should include contention, got {wait_ms} ms"
        );
        std::fs::remove_file(path).expect("remove debug JSONL");
        configure(None);
    }
}
