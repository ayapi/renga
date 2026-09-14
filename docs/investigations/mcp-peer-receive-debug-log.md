# mcp-peer receive-side debug log

Set `RENGA_DEBUG_CODEX_PEER_LOG` to a JSONL file path before starting renga.
The receive-side mcp-peer records described here and the TUI Codex peer records
are appended to that file. Every record includes `pane_id`, `process_id`,
`record_sequence`, and `timestamp_unix_ms`. When the variable is unset, the
mcp-peer does not collect the extra values or write receive-side records.

## Correlating a nudge with `check_messages`

First filter both TUI and mcp-peer records to the affected `pane_id`, then place
records whose `timestamp_unix_ms` values are within a few seconds of each other
side by side. Match each mcp-peer `check_messages` call to the first call after
the TUI's `submit_at_enter_pressed` or `tab_pressed` record. Separate records by
`process_id` before using `record_sequence`: that sequence breaks timestamp ties
only inside one process and cannot order records from different processes.

The TUI's `delivery_sequence` and the mcp-peer's `delivery_id` are different
counters. `delivery_sequence` advances per pane, while `delivery_id` comes from
the App-wide `next_peer_delivery_id`. Both look like small integers, but equal
values do not identify the same delivery. The only shared correlation data is
`pane_id` plus wall-clock time in `timestamp_unix_ms`.

## Reading receive-side records

`client_kind_resolved` also serves as the mcp-peer process-start identity
record. It includes the package `version`, executable path and modification
time, and an argument summary. Every successful mcp-peer record carries
`trace_write_failures_since_last`; an open or write failure is counted and the
next successful record reports and clears the count. A permanently unwritable
destination cannot report its own failures; the field becomes useful when a
temporary failure recovers.
The `component: "mcp_peer"` field distinguishes this process's counter and
records from TUI records appended to the same file.

The record also carries `renga_socket`, the raw `RENGA_SOCKET` value or `null`,
and `tui_pid`, parsed from the platform-specific `renga-PID` pipe or
`renga-PID.sock` filename. Keep using `renga_socket_present` to distinguish an
absent variable from a value whose endpoint shape could not be parsed. In a
machine-wide shared trace, correlate `tui_pid` with the TUI records'
`process_id` before comparing pane ids.

The TUI-side `client_kind_updated` record catalogs every actual kind write from
`kind_update_path` (`register` or `set_ready`). A refused Codex-to-Claude
downgrade instead writes `client_kind_downgrade_refused` with the same fields
plus `reason`; it does not also write `client_kind_updated`. Along with the
old/new kind and receive mode, both records carry sticky OSC-title evidence as
`pane_title_codex_seen` and `pane_title_claude_seen`. Both are `null` if the
pane cannot be found. `kind_title_mismatch` is also `null` when the pane cannot
be found or until either title has been seen, `true` when a Claude update
conflicts with a seen Codex title or a Codex update conflicts with a
Claude-only title, and `false` otherwise.

A nested registration that would previously have misregistered a Codex pane is
identified by `client_kind_downgrade_refused` with `kind_title_mismatch: true`.
An earlier
`client_kind_resolved` record with `renga_peer_client_kind_state: "absent"`
also identifies the faulty mcp-peer when its `tui_pid` and `pane_id` correlate
with that refused record. An actual `client_kind_updated` transition from Codex
to Claude means either the App had already processed `PeerSubscriberGone` for
the pane or it had not yet processed any `PeerSubscriberArrived` for that pane.

### `check_messages`

- `call_shape`: `empty`, `cursor`, or `ack` according to the supplied arguments.
- `args.message_id` and `args.offset_bytes`: the requested page cursor, or
  `null` when absent.
- `args.ack.message_id` and `args.ack.token_present`: acknowledgement metadata.
  The token value is never recorded.
- `ack_result`: `accepted`, `rejected:<reason>`, or `none`.
- `error_reason`: the JSON-RPC error text for any failed call, including an
  invalid cursor; successful calls use `null`.
- `response.head_message_id`, `count`, `pending_after`, `has_more`,
  `ack_token_present`, and `page_offset`: metadata copied from the actual tool
  response. Error responses use `null` for unavailable values.
- `response.body_len`: the byte length of the returned page for a split message,
  or the full body byte length for a complete-body response. The body and
  `total_bytes` are not recorded.
- `inbox_len_before` and `inbox_len_after`: separate, non-atomic snapshots taken
  under two short inbox locks. A concurrent push can make their difference fail
  to show a successful pop; use `ack_result` to determine acknowledgement status.

### Inbox delivery and receipt

- `peer_inbox_received`: `delivery_id`, `from_pane`, `body_len`, and the queue's
  `inbox_len_after`. The body itself is not recorded.
- `peer_inbox_ack_sent`: `delivery_id`, `ok`, and `error` after the mcp-peer asks
  the App to confirm storage or push delivery.
- `peer_inbox_ack_queued`: `delivery_id` and FIFO `depth` when the subscription
  thread hands a receipt to its dedicated sender. The sender performs blocking
  IPC outside the subscription thread, preserves order, and drains queued
  receipts when a subscription attempt disconnects before it exits.
- `peer_inbox_ack_drain_abandoned`: `pending_count` and `dropped_count` when
  subscription teardown has spent one IPC `RESPONSE_TIMEOUT` draining receipts.
  The current request may still finish on its detached IPC helper, while the
  sender discards the reported queued remainder before exiting.
  If worker creation fails and mcp-peer falls back to synchronous receipts,
  neither `peer_inbox_ack_queued` nor `peer_inbox_ack_drain_abandoned` is emitted.
- `peer_receipt_cache_hit`: `delivery_id` when a repeated event is retained
  idempotently instead of being emitted a second time.

### Push delivery timing

- `push_frame_buffered`: `delivery_id` (always present; `null` when unavailable), `frame_kind`
  (`peer_inbox`, `events_dropped`, or `other`), and `pending_len_after` when a
  notification arrives before the MCP initialized notification.
- `push_frame_emitted`: `delivery_id`, `frame_kind`, `initialized_age_ms`, and
  `via` (`direct` or `initialized_flush`) after a successful stdout write.
  The age is measured from handling the initialized notification in both paths.
- `push_frame_emit_failed`: `delivery_id`, `frame_kind`, `initialized_age_ms`,
  `via`, and `error` when a direct or initialized-flush stdout write fails.
- `push_frame_dropped_cap`: `delivery_id`, `frame_kind`, and `pending_len` when the
  pre-initialization notification queue is full.
- `push_initialized`: successful `flushed_count`, `failed_count`, and
  `subscribed_at_that_time`, timestamped after the initialization-time push
  buffer flush completes. This record precedes the corresponding
  `initialized_flush` result records in the JSONL stream.
- `push_subscribed`: `subscribed` and `initialized_at_that_time` whenever the
  event subscription state changes.
- `peer_set_ready_deferred`: `client_kind` and the remaining `delay_ms` when a
  push client has both initialized and subscribed but readiness publication is
  deferred. It is written once per nonzero scheduled delay, not while polling;
  an already elapsed delay publishes synchronously without this record.
- `peer_set_ready_sent`: `client_kind`, `ready`, `initialized_age_ms`, `ok`, and
  `error` after mcp-peer sends App a readiness update. Push initialization and
  subscription produce `ready=true`; subscription loss produces `ready=false`.
  `initialized_age_ms` is `null` when the process has not initialized yet.

### Claude startup onset measurements

Controlled T2-T4 trials on 2026-09-06 with Claude Code v2.1.261 bracketed the
startup loss after `notifications/initialized`: notifications emitted 8, 37,
38, and 54 ms afterward were lost, while notifications emitted 148 and 177 ms
or later arrived. These are one-sided samples from one Windows host in its
usual Remote Control setup, measured between 10:51 and 10:58 with no Cargo
build running, three renga TUIs, and several Codex/Claude panes. Claude startup
speed depends on host load, so concurrent builds or additional panes can make
the unsafe interval longer than 148 ms.

Push readiness therefore waits 1500 ms from initialization. This is a safety
margin, not a measured onset: about ten times the earliest successful sample
and about 28 times the latest lost sample, leaving room for startup CPU
contention and the bug's originally intermittent character. A report that a
notification still disappears after this delay should trigger new measurement
starting from the sample series above. The cost is that every Claude pane's
first peer message can be delayed by up to 1.5 seconds.

During the delay, App retains messages in `pending_peer_inbox` and does not emit
`PeerInbox`; the mcp-peer push buffer is not involved. A validating trace should
therefore contain no delay-window `push_frame_buffered` record. If the event
subscription ends before the timer fires, `ready=false` is sent immediately
and the pending `ready=true` is cancelled. The App-side no-emission invariant is
covered by `handle_peer_send_waits_for_target_peer_registration`.

### App-side ready queue timing

These records carry the TUI process id. The trace-only `peer_inbox_sequence`
assigned by `peer_inbox_queued_until_ready` is repeated in
`peer_set_ready_flush.peer_inbox_sequences`. It uses a separate per-pane
counter from Codex nudge decision records' `delivery_sequence` and does not
change the peer wire format.

- `peer_inbox_queued_until_ready`: `peer_inbox_sequence` and `queue_len_after`
  when App first retains a message for an unready pane, or requeues an
  unconfirmed delivery after receipt expiry or subscriber disconnect.
- `peer_set_ready_flush`: `client_kind`, `flushed_count`, and
  `peer_inbox_sequences` when App marks the pane ready and drains that queue.
- `peer_delivery_ready_cleared`: `reason` (`set_ready_false`,
  `unconfirmed_delivery_expired`, or `subscriber_gone`) when App revokes
  readiness explicitly, after a failed receipt, or after event subscription
  disconnect. For `subscriber_gone`, `detail` distinguishes subscription ack
  write failure, stream write failure, event-bus closure, and other internal
  teardown paths without changing the IPC wire format.
- `peer_delivery_retry_emitted`: `pane_id` and `delivery_id` when the App retries
  the oldest unacknowledged delivery for a pane.
- `peer_delivery_retry_blocked_behind_head`: `pane_id`, `delivery_id`, and
  `head_delivery_id` once when a due retry is held behind an older delivery.

### Bulk-flush receipt serialization (renga-z01)

Before this fix, a ready transition emitted an entire retained queue with the
same 100 ms retry timestamp, while mcp-peer acknowledged each event through a
blocking IPC round trip on its subscription thread. The measured round trip was
about 46 ms: a seven-message flush repeated five ids (one three times), and the
87th message of a larger flush could pass the four-second receipt timeout.
Repeated events also consumed the 256-event subscriber capacity and could
surface a misleading `EventsDropped` warning.

The App now limits retries to one oldest unacknowledged delivery per pane. Later
deliveries are emitted once during the initial FIFO flush, then cannot retry or
consume their receipt timeout while an older delivery remains. When the oldest
receipt arrives, the next queue-origin delivery receives fresh retry and expiry
timestamps. A direct caller's four-second deadline is never extended, including
when it joins an already in-flight queue-origin delivery.
In mcp-peer, a dedicated FIFO sender performs acknowledgements so the
subscription thread immediately resumes event consumption. A subscription
disconnect closes the sender and lets already queued receipts drain for at most
one IPC `RESPONSE_TIMEOUT`; after that it records and discards the queued tail.

If a queue-origin head expires, App marks the pane unready and restores all
queue-origin followers to the pre-registration FIFO at once. A follower with a
waiting direct-send reply stays in flight instead: its initial event was already
emitted, so a late acknowledgement can still complete it as `Delivered`. It is
not re-emitted while the pane is unready and returns
`peer_delivery_unconfirmed` only when its own unchanged deadline expires.
Requeued entries retain their original `delivery_id`, so a late receipt removes
the queued entry before another flush, or confirms the same id after a flush;
it never turns the already retained body into a second local delivery.

### Post-handover loss detection (renga-vtl)

For Codex pull peers, App tracking now continues after `peer_inbox_ack`: the
handover ledger retains `(delivery_id, from_pane, from_name, body, ts_ms)` until
the matching `peer_inbox_consumed` request arrives from a successful
`check_messages` head ack. Consumed reports share the asynchronous receipt FIFO;
ids whose report has not succeeded remain in a bounded process-local set.
An immediately delivered body larger than the ledger's 1 MiB cap is evicted as
soon as it is handed over, so a later disappearance cannot produce a loss
notice for that delivery.

On reconnect, `peer_inbox_reconcile` compares held and unreported-consumed ids
before readiness flushes new messages. Per-registration generations keep a
newly flushed delivery out of an older reconciliation snapshot. Missing ids,
pane closure, or a 30-second subscriber disconnect drain the applicable ledger
entries as lost. A bounded consumed-id tombstone queue remains as insurance
against unexpected command reordering. Each lost entry emits
`peer_message_lost` and routes this sender notice through normal peer delivery:

A successful reconciliation removes the reported consumed ids and clears the
corresponding overflow count. A failed reconciliation retains both for the next
connection attempt.

`Peer message to pane N was lost before it was read: its MCP peer restarted (reason: R). Sent at UNIX_SECONDS.NANOSECONDS UTC, delivery D, body began: <first 40 chars>. Resend if still needed.`

The App trace actions are `peer_handover_tracked`,
`peer_handover_consumed`, `peer_handover_lost` (with `reason` and `count`),
`peer_handover_reconciled`, `peer_handover_disconnect_timeout`, and
`peer_loss_notice_sent` (or `peer_loss_notice_dropped` when the sender no longer
exists). The mcp-peer traces `peer_inbox_consumed_unreported`,
`peer_inbox_consumed_queued`, `peer_inbox_consumed_sent`, and reconciliation
counts/overflow; the `check_messages` record also includes
`consumed_after_ack`, whose values include `sent`, `sent_sync_fallback`,
`rejected_sync_fallback`, `rejected_sender_unavailable`, and
`rejected_queue_closed`. The log remains completely silent when
`RENGA_DEBUG_CODEX_PEER_LOG` is unset. If the process dies after popping a local
head but before retaining its id in the unreported set, reconciliation can still
classify that already-read message as lost; this is the sole unavoidable
false-positive window.

### Kind downgrade refusal (renga-jvn)

`client_kind_downgrade_refused` records an incoming Claude registration or
readiness update that found both an existing Codex kind and a live peer event
stream for the pane. Its `reason` is `live_subscriber_preserves_codex`. The
attempted Claude kind and push receive mode remain in `new_client_kind` and
`receive_mode`, while `client_kind_updated` is absent because no kind write
occurred. Readiness and pending-inbox flushing still proceed for a refused
`set_ready` update, so a reachable pane does not strand queued messages.

The refusal also leaves the Codex owner's handover generation and disconnect
deadline unchanged. This keeps a nested Claude registration from making a
message delivered after the Codex peer's reconciliation snapshot look older
than the current generation and falsely reporting it through
`peer_message_lost`. Once the last subscriber is gone, a later Claude
registration is accepted, advances the generation, and writes the normal
`client_kind_updated` record.

The event bus broadcasts each peer-inbox event to every live subscription, and
each mcp-peer filters it by `target_pane`; therefore two subscribers registered
for the same pane both receive the event. This behavior is unchanged here.

Registration does not share the subscription registry lock. If a Claude
registration races the Codex stream's final disconnect, it may be refused just
before the gone notification or accepted just after it. The refused ordering
can temporarily retain the stale Codex kind until the next registration, which
is the same self-correcting stale-kind window that existed before this fix.
