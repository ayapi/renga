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
pub(crate) const PHASE_RENDER_DRAW: &str = "render_draw";
pub(crate) const PHASE_OTHER: &str = "other";
const FIELD_ACTION: &str = "action";
const FIELD_FRAME_MS: &str = "frame_ms";
const FIELD_PHASE_MS: &str = "phase_ms";
const FIELD_RENDER_BREAKDOWN_MS: &str = "render_breakdown_ms";
const FIELD_DRAW_MS_BY_COMPONENT: &str = "draw_ms_by_component";
const FIELD_PREVIEW_KIND: &str = "preview_kind";
const FIELD_PREVIEW_AREA: &str = "preview_area";
const FIELD_PREVIEW_IMAGE_REENCODED: &str = "preview_image_reencoded";
const FIELD_SIDEBAR_VISIBLE: &str = "sidebar_visible";
const FIELD_CLAUDE_MONITOR_LINES_PARSED: &str = "claude_monitor_lines_parsed";
const FIELD_CLAUDE_MONITOR_BYTES_READ: &str = "claude_monitor_bytes_read";
const FIELD_CLAUDE_MONITOR_PATH_CHANGES: &str = "claude_monitor_path_changes";
const FIELD_CLAUDE_MONITOR_LAST_MTIME_CHANGED: &str = "claude_monitor_last_mtime_changed";
const FIELD_DRAW: &str = "draw";
const FIELD_PRESENT: &str = "present";
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
    draw_by_component: BTreeMap<String, Duration>,
    preview_kind: &'static str,
    preview_area: (u16, u16),
    preview_image_reencoded: bool,
    sidebar_visible: bool,
    claude_monitor_lines_parsed: usize,
    claude_monitor_bytes_read: usize,
    claude_monitor_path_changes: usize,
    claude_monitor_last_mtime_changed: usize,
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
                preview_kind: "none",
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

pub(crate) fn record_draw_component(name: impl Into<String>, duration: Duration) {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            *frame.draw_by_component.entry(name.into()).or_default() += duration;
        }
    });
}

pub(crate) fn record_render_context(
    preview_kind: &'static str,
    preview_area: (u16, u16),
    sidebar_visible: bool,
) {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            frame.preview_kind = preview_kind;
            frame.preview_area = preview_area;
            frame.sidebar_visible = sidebar_visible;
        }
    });
}

pub(crate) fn record_preview_image_reencoded(reencoded: bool) {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            frame.preview_image_reencoded = reencoded;
        }
    });
}

pub(crate) fn record_claude_monitor_io(lines_parsed: usize, bytes_read: usize) {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            frame.claude_monitor_lines_parsed += lines_parsed;
            frame.claude_monitor_bytes_read += bytes_read;
        }
    });
}

pub(crate) fn record_claude_monitor_path_change() {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            frame.claude_monitor_path_changes += 1;
        }
    });
}

pub(crate) fn record_claude_monitor_mtime_change() {
    STATE.with(|state| {
        if let Some(frame) = state.borrow_mut().frame.as_mut() {
            frame.claude_monitor_last_mtime_changed += 1;
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

        let mut phase_ms = frame.phase_ms;
        let measured_ms: u128 = [
            PHASE_EVENT_DRAIN,
            PHASE_IPC_COMMANDS,
            PHASE_CODEX_FLUSH,
            PHASE_RENDER,
        ]
        .into_iter()
        .map(|phase| phase_ms.get(phase).copied().unwrap_or_default())
        .sum();
        let render_ms = phase_ms.get(PHASE_RENDER).copied().unwrap_or_default();
        let render_draw_ms = phase_ms.remove(PHASE_RENDER_DRAW).unwrap_or_default();
        let render_present_ms = render_ms.saturating_sub(render_draw_ms);
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
        let draw_ms_by_component: BTreeMap<String, u128> = frame
            .draw_by_component
            .into_iter()
            .map(|(name, elapsed)| (name, elapsed.as_millis()))
            .collect();
        drop(state);
        append_codex_peer_debug_record(
            OsStr::new(&path),
            json!({
                (FIELD_ACTION): ACTION_FRAME_OVER_BUDGET,
                (FIELD_FRAME_MS): frame_ms,
                (FIELD_PHASE_MS): phase_ms,
                (FIELD_RENDER_BREAKDOWN_MS): {
                    (FIELD_DRAW): render_draw_ms,
                    (FIELD_PRESENT): render_present_ms,
                },
                (FIELD_DRAW_MS_BY_COMPONENT): draw_ms_by_component,
                (FIELD_PREVIEW_KIND): frame.preview_kind,
                (FIELD_PREVIEW_AREA): {
                    "w": frame.preview_area.0,
                    "h": frame.preview_area.1,
                },
                (FIELD_PREVIEW_IMAGE_REENCODED): frame.preview_image_reencoded,
                (FIELD_SIDEBAR_VISIBLE): frame.sidebar_visible,
                (FIELD_CLAUDE_MONITOR_LINES_PARSED): frame.claude_monitor_lines_parsed,
                (FIELD_CLAUDE_MONITOR_BYTES_READ): frame.claude_monitor_bytes_read,
                (FIELD_CLAUDE_MONITOR_PATH_CHANGES): frame.claude_monitor_path_changes,
                (FIELD_CLAUDE_MONITOR_LAST_MTIME_CHANGED): frame.claude_monitor_last_mtime_changed,
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

    struct DebugEnvRestore(Option<OsString>);

    impl DebugEnvRestore {
        fn set(path: &OsStr) -> Self {
            let previous = std::env::var_os("RENGA_DEBUG_CODEX_PEER_LOG");
            std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", path);
            crate::app::set_codex_peer_debug_log_path_test_override(None);
            Self(previous)
        }
    }

    impl Drop for DebugEnvRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", value),
                None => std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG"),
            }
            crate::app::set_codex_peer_debug_log_path_test_override(Some(None));
            configure(None);
        }
    }

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
        let render_started_at = phase_started();
        std::thread::sleep(Duration::from_millis(2));
        let render_draw_started_at = phase_started();
        std::thread::sleep(Duration::from_millis(10));
        record_draw_component("tabs", Duration::from_millis(1));
        record_draw_component("pane:3", Duration::from_millis(1));
        record_draw_component("pane:3", Duration::from_millis(1));
        record_draw_component("file_tree", Duration::from_millis(1));
        record_draw_component("claude_monitor", Duration::from_millis(1));
        record_draw_component("macos_tip", Duration::from_millis(1));
        record_draw_component("preview", Duration::from_millis(1));
        record_draw_component("status_bar", Duration::from_millis(1));
        record_draw_component("overlay", Duration::from_millis(1));
        record_render_context("image", (42, 17), true);
        record_preview_image_reencoded(true);
        record_claude_monitor_io(2, 100);
        record_claude_monitor_io(3, 250);
        record_claude_monitor_path_change();
        record_claude_monitor_path_change();
        record_claude_monitor_mtime_change();
        finish_phase(PHASE_RENDER_DRAW, render_draw_started_at);
        std::thread::sleep(Duration::from_millis(2));
        finish_phase(PHASE_RENDER, render_started_at);
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
        assert_eq!(
            record[FIELD_PHASE_MS]
                .as_object()
                .expect("phase milliseconds")
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            [
                PHASE_EVENT_DRAIN,
                PHASE_IPC_COMMANDS,
                PHASE_CODEX_FLUSH,
                PHASE_RENDER,
                PHASE_OTHER,
            ]
            .into_iter()
            .collect()
        );
        let render_ms = record[FIELD_PHASE_MS][PHASE_RENDER]
            .as_u64()
            .expect("render milliseconds");
        let render_draw_ms = record[FIELD_RENDER_BREAKDOWN_MS][FIELD_DRAW]
            .as_u64()
            .expect("render draw milliseconds");
        let render_present_ms = record[FIELD_RENDER_BREAKDOWN_MS][FIELD_PRESENT]
            .as_u64()
            .expect("render present milliseconds");
        assert!(render_draw_ms > 0);
        assert!(render_present_ms > 0);
        assert_eq!(
            record["draw_ms_by_component"],
            json!({
                "claude_monitor": 1,
                "file_tree": 1,
                "macos_tip": 1,
                "overlay": 1,
                "pane:3": 2,
                "preview": 1,
                "status_bar": 1,
                "tabs": 1,
            })
        );
        let component_ms: u64 = record["draw_ms_by_component"]
            .as_object()
            .expect("draw component milliseconds")
            .values()
            .map(|value| value.as_u64().expect("component milliseconds"))
            .sum();
        assert!(
            component_ms <= render_draw_ms,
            "disjoint component measurements must fit inside draw: components={component_ms}, draw={render_draw_ms}"
        );
        assert_eq!(record["preview_kind"], "image");
        assert_eq!(record["preview_area"], json!({"w": 42, "h": 17}));
        assert_eq!(record["preview_image_reencoded"], true);
        assert_eq!(record["sidebar_visible"], true);
        assert_eq!(record["claude_monitor_lines_parsed"], 5);
        assert_eq!(record["claude_monitor_bytes_read"], 350);
        assert_eq!(record["claude_monitor_path_changes"], 2);
        assert_eq!(record["claude_monitor_last_mtime_changed"], 1);
        assert!(
            render_ms.abs_diff(render_draw_ms + render_present_ms) <= 1,
            "render subphases should add to render: render={render_ms}, draw={render_draw_ms}, present={render_present_ms}"
        );
        assert_eq!(
            record[FIELD_PHASE_MS][PHASE_OTHER].as_u64(),
            Some((FRAME_OVER_BUDGET_MS + 1) as u64 - render_ms),
            "render breakdown must not change other phase accounting"
        );
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
    fn production_configuration_writes_when_enabled_and_not_when_disabled() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = temp_log_path("production-env");
        let _env = DebugEnvRestore::set(path.as_os_str());

        configure_from_env();
        let started_at = Instant::now() - Duration::from_millis(FRAME_OVER_BUDGET_MS as u64 + 1);
        begin_frame(started_at);
        finish_frame(|| vec![11]);
        let contents = std::fs::read_to_string(&path).expect("enabled production-path JSONL");
        assert_eq!(contents.lines().count(), 1);
        let record: Value = serde_json::from_str(contents.trim()).expect("one JSON object");
        assert_eq!(record[FIELD_PREVIEW_KIND], "none");
        assert_eq!(record[FIELD_PREVIEW_AREA], json!({"w": 0, "h": 0}));
        assert_eq!(record[FIELD_PREVIEW_IMAGE_REENCODED], false);
        assert_eq!(record[FIELD_SIDEBAR_VISIBLE], false);
        assert_eq!(record[FIELD_CLAUDE_MONITOR_LINES_PARSED], 0);
        assert_eq!(record[FIELD_CLAUDE_MONITOR_BYTES_READ], 0);
        assert_eq!(record[FIELD_CLAUDE_MONITOR_PATH_CHANGES], 0);
        assert_eq!(record[FIELD_CLAUDE_MONITOR_LAST_MTIME_CHANGED], 0);
        std::fs::remove_file(&path).expect("remove enabled production-path JSONL");

        std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG");
        configure_from_env();
        let started_at = Instant::now() - Duration::from_millis(FRAME_OVER_BUDGET_MS as u64 + 1);
        begin_frame(started_at);
        finish_frame(|| vec![11]);
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
