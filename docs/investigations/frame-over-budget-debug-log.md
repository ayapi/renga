# Frame over-budget debug log

Set `RENGA_DEBUG_CODEX_PEER_LOG` to a JSONL file path before starting renga.
The existing Codex peer records and the frame records described here are
appended to that file. Every record includes `process_id`, `record_sequence`,
and `timestamp_unix_ms`. When the variable is unset, frame details and lock
timestamps are not collected.

Test binaries do not read the inherited environment variable under `cfg(test)`;
only tests that inject a path directly write debug records. Traces captured on
or before 2026-09-05 can still contain earlier test contamination, identifiable
by a short-lived `process_id`, roughly 167 records per process, and pane ids 1
through 3. The `mcp_peer` subprocess path remains outside this isolation and
still reads the variable directly, but that path is not executed by the test
binary.

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
