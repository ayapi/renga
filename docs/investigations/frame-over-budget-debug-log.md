# Frame over-budget debug log

Set `RENGA_DEBUG_CODEX_PEER_LOG` to a JSONL file path before starting renga.
The existing Codex peer records and the frame records described here are
appended to that file. Every record includes `process_id`, `record_sequence`,
and `timestamp_unix_ms`. When the variable is unset, frame details and lock
timestamps are not collected.

Test binaries do not normally read the inherited environment variable under
`cfg(test)`; tests instead inject a path directly. Two wiring tests temporarily
set the variable to disposable paths for the TUI and mcp-peer paths, serialized
by a shared static mutex so their process-wide mutations cannot race. Traces
captured on or before 2026-09-05 can still
contain earlier test contamination, identifiable by a short-lived `process_id`,
roughly 167 records per process, and pane ids 1 through 3. The `mcp_peer`
subprocess resolves the variable once at startup and its unit tests inject the
resolved path into `PeerCtx`; they do not start the subprocess.

## Process lifecycle and liveness records

- `process_start`: emitted once for a TUI launch, with `version`,
  `executable_path`, `executable_modified_unix_ms`, and a non-payload
  `args_summary`.
- `heartbeat`: emitted from the end of an existing event-loop iteration about
  every 60 seconds. It contains `frames_since_last`, total `pane_count`,
  `visible_tab`, and `trace_write_failures_since_last`; it does not request an
  extra render.
- `process_exit`: emitted after App shutdown and before terminal cleanup, with
  `reason`, an optional event-loop `error`, `frames_total`, and `uptime_ms`.
  Uptime starts immediately before `process_start` and event-loop entry.
- `panic`: emitted by the process panic hook before the default hook, with the
  panic `message`, `thread_name`, and source `location`.

Open or write failures increment an in-process counter. The next successful
heartbeat reports and clears that count. A permanently unwritable destination
cannot report its own failure count; this field diagnoses temporary failures
after writing recovers. As with every record in this file, none of these
records are written when `RENGA_DEBUG_CODEX_PEER_LOG` is unset.
All App-side records include `component: "tui"`. Setup failures before the
event loop and panics before the terminal-restoration hook is installed cannot
be recorded by these lifecycle entries. Terminal-cleanup failures happen after
`process_exit` and retain the pre-existing return behavior.

## Threshold and record names

`FRAME_OVER_BUDGET_MS` is 500 ms. A loop iteration whose elapsed time is
strictly greater than the threshold writes one `frame_over_budget` record.
Each IPC command also writes one `ipc_command_processed` record regardless of
the frame threshold.

## Reading `frame_over_budget`

- `frame_ms`: total event-loop iteration time, including input polling.
- `phase_ms`: elapsed milliseconds in `event_drain`, `ipc_commands`,
  `codex_flush`, `render`, and `other`. `other` is the total less the four
  explicitly measured phases.
- `render_breakdown_ms`: `draw` is the time spent building the ratatui buffer
  inside `ui::render`; `present` is the remainder of `render`, including
  terminal diff output, flush, and cursor operations.
- `draw_ms_by_component`: disjoint portions of `ui::render`, grouped as
  `pane:<id>`, `claude_monitor`, `file_tree`, `preview`, `tabs`, `status_bar`,
  `macos_tip`, and `overlay` (IME or Codex peer notification).
  Their sum does not exceed `render_breakdown_ms.draw`; uncategorized layout
  and background work accounts for any remainder.
- `preview_kind`: the displayed preview type: `image`, `text`, `binary`, or
  `none`.
- `preview_area`: the displayed preview content dimensions as `w` and `h`, or
  zeroes when no preview is displayed.
- `preview_image_reencoded`: whether ratatui-image reported that the displayed
  image needed resizing and encoding immediately before this frame rendered
  it. False for non-image previews and while image rendering is skipped during
  a drag.
- `sidebar_visible`: whether the file-tree sidebar was displayed in the frame.
- `claude_monitor_lines_parsed` and `claude_monitor_bytes_read`: complete
  transcript lines passed to the Claude event parser and bytes read from
  Claude JSONL files during the frame. Bytes include a trailing incomplete
  line even though that line is not passed to the parser.
- `claude_monitor_path_changes`: number of panes whose selected Claude JSONL
  path changed during the frame. A nonzero value together with large monitor
  byte/line counts distinguishes a full reread after a path change from an
  ordinary incremental read.
- `claude_monitor_last_mtime_changed`: number of panes where the selected
  transcript's metadata modification time differed from the prior check.
- `events_drained`: all `AppEvent` values drained in the iteration.
- `pty_output_events`: the subset of drained events carrying PTY output.
- `ipc_commands`: processed commands with `command`, resolved `pane_id`,
  `queue_wait_ms`, and `command_ms`.
- `pty_writes`: calls to `write_input_to_pane`, grouped by `pane_id` with a
  `count`.
- `output_bytes_by_pane`: bytes reported by PTY reader events, grouped by pane.
- `lock_wait_ms_by_pane`: total time waiting to acquire parser locks during
  rendering, pane inspection, and Codex screen snapshots, grouped by pane.
- `visible_panes`: pane ids in the active workspace layout at frame end.

A large `render` value together with a large `lock_wait_ms_by_pane` entry
identifies a visible pane whose parser lock delayed the App thread. A large
`event_drain` value with high `pty_output_events` and `output_bytes_by_pane`
instead points to event-channel work.

A large `render_breakdown_ms.present` with a small `draw` points to the host
console or terminal-output path rather than ratatui buffer construction.
A large `draw` with a small `present` instead points to work inside
`ui::render`, such as pane painting, Claude transcript monitoring, the file
tree, or preview rendering, rather than the host console.

Use `draw_ms_by_component` to distinguish pane, Claude transcript monitoring,
file-tree, preview, tab, status-bar, macOS tip, and IME/peer-notification costs.
Each `pane:<id>` entry covers `render_single_pane` only. Layout calculation,
pane resizing, monitor cwd collection, background painting, and other
uncategorized work remain in `render_breakdown_ms.draw` minus the component
sum. For image previews,
`preview_image_reencoded: true` directly identifies a resize-and-encode frame;
this uses the read-only API exposed by ratatui-image 10.0.6 rather than an
estimate from elapsed time.

When a frame does not draw because `app.dirty` is false, `preview_kind` remains
`"none"`, `sidebar_visible` remains false, and `preview_area` remains zero.
Such records have `render_breakdown_ms.draw == 0` and an empty
`draw_ms_by_component` map. On NTFS, modification-time updates for a transcript
that remains open may be delayed until a later flush or handle close, so the
monitor's metadata check may not observe every incremental write immediately.

Parser-lock acquisition in `keyboard_input.rs` (lines 515 and 664),
`pointer_input.rs` (line 181), and the user-input-only scroll methods in
`pane.rs` is not measured. A large `other` value with an empty
`lock_wait_ms_by_pane` therefore does not rule out parser-lock contention.

`timestamp_unix_ms` is captured when the frame record is written at frame end;
subtract `frame_ms` to estimate the frame start time.

`pty_writes` covers `write_input_to_pane` calls used for peer and IPC nudges.
It does not include the keyboard path through `flush_paste_buffer`.

## Reading `ipc_command_processed`

- `enqueued_at_ms`: Unix milliseconds captured by the IPC worker immediately
  before it enqueued the command.
- `queue_wait_ms`: time from enqueue to the App thread starting the handler.
- `command_ms`: time spent in the App handler.
- `command` and `pane_id`: command kind and resolved target pane. Commands
  without a pane target use `null`.

For a batch of `inspect_pane` calls, compare `enqueued_at_ms` and
`queue_wait_ms` with the slow frame. A command that arrived during an App
stall has a large queue wait; an inspection that blocks while acquiring its
target parser lock has a large handler time and a matching pane entry in
`lock_wait_ms_by_pane`.
