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
- `peer_receipt_cache_hit`: `delivery_id` when a repeated event is retained
  idempotently instead of being emitted a second time.

### Push delivery timing

- `push_frame_buffered`: `delivery_id` when available, `frame_kind`
  (`peer_inbox`, `events_dropped`, or `other`), and `pending_len_after` when a
  notification arrives before the MCP initialized notification.
- `push_frame_emitted`: `delivery_id`, `frame_kind`, `initialized_age_ms`, and
  `via` (`direct` or `initialized_flush`) after a successful stdout write.
  Flush records use an age of zero.
- `push_frame_dropped_cap`: `frame_kind` and `pending_len` when the
  pre-initialization notification queue is full.
- `push_initialized`: `flushed_count` and `subscribed_at_that_time` when the MCP
  initialized notification is handled. This record precedes the corresponding
  `initialized_flush` emission records.
- `push_subscribed`: `subscribed` and `initialized_at_that_time` whenever the
  event subscription state changes.
- `peer_set_ready_sent`: `client_kind` when mcp-peer sends App the readiness
  update after both push initialization and subscription have completed.

### App-side ready queue timing

These records carry the TUI process id. The trace-only `delivery_sequence`
assigned by `peer_inbox_queued_until_ready` is repeated in
`peer_set_ready_flush.delivery_sequences`; it does not change the peer wire
format.

- `peer_inbox_queued_until_ready`: `delivery_sequence` and `queue_len_after`
  when App retains a message for a pane that has not reported ready.
- `peer_set_ready_flush`: `client_kind`, `flushed_count`, and
  `delivery_sequences` when App marks the pane ready and drains that queue.
- `peer_delivery_ready_cleared`: `reason` (`unconfirmed_delivery_expired` or
  `subscriber_gone`) when App revokes readiness after a failed receipt or event
  subscription disconnect.
