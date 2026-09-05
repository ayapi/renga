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
