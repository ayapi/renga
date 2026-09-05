# renga v1.0 — Frozen API Surface

> **Status**: design proposal. Defines the surface that v1.0 promises to keep
> stable under the rules in [`semver-policy.md`](./semver-policy.md).
>
> **Source-of-truth commit**: `main` at the time this doc lands; cross-checked
> against `Cargo.toml` `version = "0.18.5"` (the last pre-1.0 release).
>
> **Audience**: downstream callers (claude-org-ja workers, Codex peers,
> third-party tooling that integrates with `renga-peers`).

## Stability legend

- **stable** — frozen by v1.0. Breaking changes follow the deprecation window
  in [`semver-policy.md`](./semver-policy.md).
- **stable-stub** — frozen as a no-op for wire-compat. Caller-visible inputs
  and outputs are part of the contract; behavior can be upgraded additively
  (a stub becoming a real implementation is *not* a break).
- **deferred** — shipped today, intentionally **not** part of v1.0. May change
  shape in any minor release; downstream should not depend on it.

The four frozen surfaces are: **MCP tools** (§1), **CLI** (§2),
**IPC protocol** (§3), and **config / layout / env** (§4). Errors and
forward-compat rules are in §5.

---

## 1. MCP tools (`renga-peers` stdio MCP server)

Launched per pane as `renga mcp-peer`. Speaks MCP over stdio. Routes peer
traffic through the renga IPC server.

**Frozen contract — `serverInfo`**:

- `name` is `"renga-peers"` — **stable**. Claude Code derives the
  `<channel source="renga-peers">` notification tag from this string. Renaming
  it is a breaking change (Q8).
- `version` follows `CARGO_PKG_VERSION`. Not part of the wire contract beyond
  semver compliance.

**Frozen contract — detached mode**: when `RENGA_PANE_ID` / `RENGA_SOCKET` are
absent the server still handshakes and advertises the full tool list. Tools
return ok-text payloads with the prefixes documented per tool below; clients
must accept these instead of JSON-RPC errors.

### 1.1 `list_peers` — stable

| Field | Type | Notes |
|---|---|---|
| `scope` (in) | `"machine"\|"directory"\|"repo"` | Optional; **ignored**. Accepted for wire-compat with `claude-peers-mcp`. renga always treats scope as the current tab. |

Result: text content listing `id`, `name`, `role`, `kind` (`claude`/`codex`),
`receive_mode` (`push`/`pull`), `cwd`. Empty case: `"No peers in this tab."`.

**Detached fallback (frozen prefix)**: `"(no peers — renga not reachable from
this peer client: <reason>)"`. Downstream may match on this prefix; it is part
of the wire ABI.

### 1.2 `send_message` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `to_id` | string | yes | Recipient pane id (numeric string) or stable name. All-digit strings are interpreted as ids (see §6.1). |
| `message` | string | yes | Body text. |

Result on immediate success: `"Delivered to <to_id>."`. If the target pane
exists in the same tab but its peer event subscriber is not ready, the result
is `"Queued for <to_id> (peer client not registered yet)."`; delivery occurs
after that subscriber reports readiness, in original send order.
If a ready Codex target requires user acceptance before its pane-local nudge
can be delivered, the result is `"Pending user confirmation for <to_id>."`.
If the target does not resolve in the caller's tab, the result is
`"Not delivered to <to_id>: no such pane in this tab (pane ids and names are tab-scoped)."`.
If the server omits `delivery` or returns an unknown value, the result is
`"Message sent to <to_id>; delivery state unconfirmed (renga server may predate queued delivery)."`.
The server is authoritative for this state; during a client/server version
skew, a new client displays the outcome reported by the older server.

The pre-registration queue is per pane and bounded to 128 messages / 1 MiB of
body text. A send that would exceed either cap fails with
`peer_queue_full` instead of claiming successful retention.
The 1 MiB cap intentionally applies only while the target is not ready,
because queued bodies consume App memory; an immediately delivered body is
not retained there and therefore does not consume that queue budget.

**Detached fallback (frozen prefix)**: `"(message dropped — renga not
reachable: <reason>)"`.

**Unresolved and cross-tab targets**: both return
`{ "delivery": "undeliverable" }`, a success-shaped result that queues no body.
The response does not reveal whether the pane exists in another tab (Q5).
Adding the `undeliverable` value is additive. Relabeling these formerly
successful no-delivery cases is an observable behavior change that is
intentionally accepted: the previous contract already said no delivery
occurred, so its `Delivered` label was not reliable confirmation.

**Same-payload dedupe (post-1.1)**: identical `(target, sender, body)` triples
arriving within a small dedupe window (~5s) are collapsed server-side to a
single delivery. A repeat call returns the same outcome as the original call:
`"Delivered to …"` for an immediately delivered message, or `"Queued for …"`
for a message accepted into the peer's pending queue, or
`"Pending user confirmation for …"` when the original nudge awaits acceptance.
This prevents the sender from probing the dedupe state; only one
`Event::PeerInbox` reaches the receiver.
Two distinct senders sending the same body still both deliver. See renga#221
for context.

**Receiver retention confirmation (post-1.3)**: for both Claude push and Codex
pull recipients, `Delivered` is returned only after the target `renga mcp-peer`
has retained the message locally and acknowledged its `delivery_id`. The App
retries a dropped event with the same id; the peer deduplicates that id before
acknowledging again. If no receipt arrives before the sender reply deadline,
the call fails with `peer_delivery_unconfirmed` and may be retried immediately.
A new App paired with an older mcp-peer receives no receipt and therefore
returns this timeout error on direct delivery, never an unverified `Delivered`
result. A queued pre-registration message is retained after its flush times out;
the pane becomes unready, so later sends remain `Queued` until the event stream
reconnects and publishes readiness again.

**Push-mode body banner (post-1.1)**: for Claude (push) recipients renga
prepends a `📡 PEER MESSAGE — from {name} (id={id}) — NOT FROM USER` line
to the body before pushing it as `notifications/claude/channel`. The original
body is preserved verbatim after a blank line; pull-mode (Codex) deliveries
are unaffected. See renga#221.

Errors via `[code]`: `pane_not_found`, `pane_vanished`, `io_error`,
`peer_delivery_unconfirmed`, plus the shared `app_timeout` / `shutting_down` /
`internal` set.

### 1.3 `set_summary` — stable (Q1)

| Field | Type | Required |
|---|---|---|
| `summary` | string | yes |

**v1.0 contract**: input shape is frozen; the tool is **implemented and
shipped in v1.0** (no longer a stub).

**Behavior**: the summary string is stored on the calling pane (resolved
from `RENGA_PANE_ID`) and surfaced as `summary` on every `PaneInfo` /
`PeerInfo` entry returned by `list_panes` and `list_peers`. Storage is
in-memory only — does not persist across renga restarts.

- An empty string clears the summary (round-trips to `Option::None` /
  omitted key on the wire).
- Repeated calls overwrite the previous value with the latest.
- Maximum length is 256 Unicode scalar values (`chars()`, not bytes);
  oversized input is rejected with `[summary_too_long]` before any
  state mutation.
- Pane exit drops the summary alongside the rest of the pane state.

Errors via `[code]`: `summary_too_long`, plus the shared
`pane_not_found` / `pane_vanished` / `app_timeout` / `shutting_down` /
`internal` set.

### 1.4 `check_messages` — stable

Reads one size-limited page from the local pull inbox. Reading never removes a
message; only an explicit receipt acknowledgement removes the FIFO head.

| Input field | Type | Required | Notes |
|---|---|---|---|
| `max_response_bytes` | integer 1..=1048576 | no | Maximum serialized JSON-RPC response size. Default `4096`. This is a transport page size, **not** a measured Codex context threshold; if a client still truncates the response, retry the same cursor without ack using a smaller value. |
| `message_id` | string | no | Opaque id from `delivery.message_id`. Echo it when requesting a continuation page. |
| `offset_bytes` | non-negative integer | no | UTF-8 byte position from `delivery.next_offset_bytes`. Default `0`; a non-zero value requires `message_id`. |
| `ack` | `{message_id, token}` | no | Confirms complete receipt, not completion of the requested work. Cannot be combined with cursor fields. A valid ack removes that FIFO head and returns only an acknowledgement; it never returns the next head. |

Every successful result retains `content`, `structuredContent.messages[]`,
`count`, and `isError`. It always adds `pending_after`, `has_more`, and
`ack_required`. A successful ack also adds `acknowledged_message_id`.
`structuredContent.delivery` is added only when a call reads a FIFO head, never
to an ack response:

- When the complete body fits at offset 0, `messages[0]` retains the v1.0 entry
  shape (`from_id`, `from_name`, `from_kind`, `body`, `sent_at`) and `count=1`.
  `delivery.body_in_messages=true` points to that sole body copy.
- When a body needs pages, `messages=[]` and `count=0`, so an older consumer
  cannot mistake a fragment for a complete instruction. `delivery.body_chunk`
  carries the page along with sender metadata, `message_id`, `offset_bytes`,
  `next_offset_bytes`, `total_bytes`, and `complete`.
- The final page includes `delivery.ack_token` and sets `ack_required=true`.
  The message remains queued until a later call supplies that token in `ack`.
- `pending_after` is the number of messages waiting behind the unacknowledged
  head. `has_more` stays true while any head remains, including a fully returned
  head that still needs ack.
- A successful ack returns `messages=[]`, `count=0`, `ack_required=false`, and
  no `delivery`. Its `acknowledged_message_id` identifies the removed head;
  `pending_after` is the number still queued and `has_more` is whether that
  number is non-zero.
- `content` contains concise handling guidance and metadata only. The peer body
  appears once under `structuredContent`, avoiding the previous text/structured
  duplication.

Caller loop: read `messages[0].body` when present; otherwise append
`delivery.body_chunk` and call again with the returned `message_id` and
`next_offset_bytes`. Do not act on a partial body. Once `complete=true` and the
entire body is assembled, call again with `ack {message_id, token}`. A response
lost or truncated before ack is idempotently retrievable with the same cursor.
The ack response contains confirmation only. The next message is announced by
its own nudge and is read with a fresh `check_messages({})` call; if the ack
response already reports `pending_after > 0`, make that fresh call immediately.
The serialized frame includes the JSON-RPC request id, so changing the id's
digit count can move a retried page's final character; following each returned
`next_offset_bytes` still reconstructs the body without loss.
Peers that never ack keep the head for the MCP subprocess lifetime; renga does
not expire it or repeatedly nudge the pane automatically.

### 1.5 `list_panes` — stable

Input: `{}`.

Result: text describing every pane in the **current tab** (Q4): `id`, `name`,
`role`, `focused`, geometry (`x`, `y`, `width`, `height`), `cwd`, `kind`,
`receive_mode`. Geometry fields are `0` before the first layout pass.

### 1.6 `spawn_pane` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `direction` | `"vertical"\|"horizontal"` | yes | `vertical` → new pane on the right; `horizontal` → new pane on the bottom. |
| `target` | string | no | Numeric id, name, or `"focused"`. Default `"focused"`. |
| `command` | string | no | Startup command. **Bare `claude [...]` is auto-rewritten to the Alt+P peer-enabled form** — see contract note below (Q3). |
| `name` | string | no | Stable pane name, must satisfy `[A-Za-z0-9_-]`, not all-digits. |
| `role` | string | no | Free-form label. Non-unique. When `command` is omitted and `role` is exactly `"claude"`, renga queues the peer-enabled Claude startup command for automatic execution; an explicit `command` takes precedence. |
| `cwd` | string | no | Absolute or relative-to-caller. Validated **before** layout mutation; failure is `cwd_invalid`. |

Returns: text containing the new pane's numeric id.
For `spawn_pane`, `spawn_claude_pane`, and `spawn_codex_pane`, the response
confirms pane creation. When an effective startup command exists, the response
confirms command queueing only; process startup is asynchronous and is not yet
confirmed, so allow startup time and use `inspect_pane` to verify.

**`command` rewrite contract (Q3)**: when `command` starts with the bare token
`claude` (no `--dangerously-load-development-channels`), renga injects the
peer-enabled launch flags so the new Claude pane joins the renga-peers channel
and skips the per-launch permission prompt — currently
`--dangerously-load-development-channels server:renga-peers --permission-mode
bypassPermissions` (renga-234). Callers that want a different permission mode
can append their own `--permission-mode <value>`; Claude CLI's later-flag-wins
resolution lets it override the baseline without renga having to strip it. An
explicit `--dangerously-load-development-channels` is left alone. This is
**frozen behavior**; no opt-out flag in v1.0. Callers that want verbatim
execution should pick a different leading token (e.g. `bash -c 'claude ...'`).

Errors: `split_refused` (MAX_PANES = 16, or below `min_pane_width` /
`min_pane_height`), `cwd_invalid`, `pane_not_found`, `name_in_use`,
`name_invalid`, `io_error`.

### 1.7 `spawn_claude_pane` — stable

Both specialized tools (§1.7 and §1.8) always send their generated startup
command, so the `role: "claude"` default documented in §1.6 does not apply to
either tool.

`spawn_claude_pane` has the same envelope as `spawn_pane` minus `command`, plus
these structured Claude fields:

| Field | Type | Required | Notes |
|---|---|---|---|
| `direction`, `target`, `name`, `role`, `cwd` | as in §1.6 | direction yes | |
| `permission_mode` | string | no | Rendered as `--permission-mode <v>`. Not enum-validated server-side, so new Claude permission modes work without a renga release. |
| `model` | string | no | Rendered as `--model <v>`. |
| `args` | string[] | no | Appended after structured fields. Must NOT contain `--dangerously-load-development-channels` / `--permission-mode` / `--model` — rejected with JSON-RPC `-32602` invalid-params. |

POSIX shell quoting is applied server-side. Values containing single quotes may
not round-trip cleanly on PowerShell-fallback Windows hosts; callers should
restrict structured-field values to `[A-Za-z0-9_./:@+%=-]` for safety.

### 1.8 `spawn_codex_pane` — stable

Same envelope as `spawn_claude_pane` minus `permission_mode` / `model`. `args`
is appended after the literal `codex` token.

**Pre-condition**: the user must have run `renga mcp install --client codex`
so `RENGA_PEER_CLIENT_KIND=codex` is injected into Codex's MCP subprocess env.
The handler verifies this up front by inspecting `~/.codex/config.toml` for
`[mcp_servers.renga-peers.env] RENGA_PEER_CLIENT_KIND = "codex"`. If the file
is missing/unreadable, the renga-peers entry is absent, or the value differs
from `"codex"`, the call returns a JSON-RPC `-32603` whose message carries the
`[codex_not_installed]` marker and the remediation hint
`renga mcp install --client codex`. Issue #203 — replaces the prior
silent-bifurcation behavior recorded in v1.0.

### 1.9 `close_pane` — stable

Input: `{ target: string }` (required).

Errors: `pane_not_found`, `pane_vanished`, `last_pane` (only pane of only
remaining tab — surfaced as an error, not silenced), `io_error`.

### 1.10 `focus_pane` — stable

Input: `{ target: string }` (required). `"focused"` is a no-op (kept for
symmetry). Yanking focus is user-disruptive; doc explicitly tells callers to
use sparingly.

### 1.11 `new_tab` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `command` | string | no | Same `claude` auto-rewrite as `spawn_pane`. |
| `name` | string | no | Stable pane name for the new tab's initial pane. |
| `label` | string | no | Tab label override (default: derived from cwd). |
| `role` | string | no | Free-form. When `command` is omitted, `"claude"` selects the same peer-enabled Claude startup command as `spawn_pane`; the command is queued and process start is not confirmed. |
| `cwd` | string | no | Absolute or relative-to-caller. Defaults to the renga server's cwd. |

Returns: numeric pane id of the new tab's initial pane. Focus switches to the
new tab. The success text also reports the effective startup command from the
server: a string means the command was queued but process start is not yet
confirmed, explicit `null` confirms no command was requested, and a missing or
non-string value is reported as unconfirmed for compatibility with older
servers. When neither `command` nor `role` was supplied, no startup command is
possible and an older server's missing key is reported as no command requested.

### 1.12 `inspect_pane` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | string | yes | |
| `lines` | int ≥ 1 | no | Bottom N rows (blank rows preserved). |
| `include_cursor` | bool | no | Default `false`. |
| `format` | `"text"\|"grid"` | no | Default `"text"`. `"grid"` returns one JSON row object per line. `structuredContent` is always populated regardless of `format`. |

### 1.13 `send_keys` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | string | yes | |
| `text` | string | no | Sent before `keys`. |
| `keys` | string[] | no | See key vocabulary below. |
| `enter` | bool | no | Append CR after `text` + `keys`. |

**Frozen key vocabulary**: `Enter`/`Return`, `Tab`, `Shift+Tab`/`BackTab`,
`Esc`/`Escape`, `Backspace`, `Delete`/`Del`, `Up`/`Down`/`Left`/`Right`,
`Home`/`End`, `PageUp`/`PageDown`, `Space`, `Ctrl+<A-Z>`. Unknown names return
JSON-RPC `-32602` invalid-params **before** the IPC call (so detached-mode
rejection is also pre-IPC).

PTY-byte semantics — *not* a logical message. Whatever process is running in
the pane sees the bytes.

### 1.14 `set_pane_identity` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | string | no | Default `"focused"`. |
| `name` | string \| null | no | **Three-state**: omit = leave; `null` = clear; string = set. |
| `role` | string \| null | no | Same three-state semantics. |

Validation: `name` non-empty, not all-digits, `[A-Za-z0-9_-]` only, unique
within tab.

Errors: `name_in_use`, `name_invalid`, `pane_not_found`.

Returns the updated pane record so the caller doesn't need a `list_panes`
round-trip.

### 1.15 `poll_events` — stable (Q2)

| Field | Type | Required | Notes |
|---|---|---|---|
| `since` | string | no | Opaque cursor returned by a prior `next_since`. Omit → "start at now"; no historical replay. |
| `timeout_ms` | int ≥ 0 | no | Default 2000, hard cap 30000. `0` = non-blocking drain. |
| `types` | string[] | no | Filter list. Cursor advances past filtered-out events; non-matching arrival can early-return with `events: []` and an advanced cursor. |

Returns: `{ next_since: <cursor>, events: [<event obj>] }`.

Buffer cap: 4096 events per process; older entries evicted on overflow with an
`events_dropped` meta-event flowing through the buffered stream.

**Contract note (Q2)**: `poll_events` is the **MCP-side, opaque-cursor**
event interface — the right tool when a peer wants pull-style polling with
filters. The CLI `renga events` command (§2.2) is the **subscribe-stream**
counterpart for shell pipelines. Both are first-class in v1.0 and serve
different use cases; neither is deprecated.

**Counterintuitive but frozen**: a poll that filters out every buffered event
returns `events: []` with an **advanced** cursor. Callers must re-poll on
empty responses to make progress.

### Common error wire format

JSON-RPC error `message` is `[<code>] <human message>` per `fmt_code`. Codes
are sourced from `renga::ipc::err_code` and are stable per its module-level
"Stability" doc-comment (deprecation-window contract — see §5).

JSON-RPC numeric codes (Q9): the renga MCP layer uses `-32602` for
client-side input validation faults (empty `to_id`, unknown `send_keys` key
name, conflicting `spawn_claude_pane.args` flag, unknown `inspect_pane.format`)
and `-32603` for everything else carrying a `[code]`. v1.0 freezes the
current usage but does **not** standardize a finer split — future minor
releases may narrow `-32603` cases to more specific numeric codes; downstream
must continue to read the `[code]` token for branching.

---

## 2. CLI surface (`renga` binary)

`renga [DIR] [flags] [SUBCOMMAND]`. With no subcommand the TUI launches; a
subcommand always exits without starting the TUI (dispatched over IPC to an
already-running renga server).

### 2.1 Top-level invocation — stable

| Arg / flag | Value | Notes |
|---|---|---|
| `DIR` (positional) | path | cwd to switch into before launching the TUI. |
| `--exec <CMD>` | string | Auto-run in initial pane. Conflicts with `--layout`. |
| `--layout <NAME>` | string | Load `./renga-layouts/<NAME>.toml` or `~/.config/renga/layouts/<NAME>.toml` (or `$RENGA_LAYOUTS_DIR`). Conflicts with `--exec`. |
| `--ime <hotkey\|off>` | enum | Overrides `[ime] mode` in config. |
| `--ime-freeze-panes[=BOOL]` | bool | Suppress repaints while IME overlay open. Default `true`. |
| `--ime-overlay-catchup-ms <MS>` | u64 | Periodic repaint interval while frozen. Default 3000, clamped ≥ 100; `0` = pure freeze. |
| `--lang <auto\|ja\|en>` | enum | UI language. |
| `--min-pane-width <COLS>` | u16 | Default 20. `0` clamps to 1. Process-global; not exposed per-call (see §6 *Out of scope*). |
| `--min-pane-height <ROWS>` | u16 | Default 5. `0` clamps to 1. Same scope as `--min-pane-width`. |
| `--no-macos-tip` / `--show-macos-tip` | bool | macOS Option-as-Meta first-launch banner override. Mutually exclusive. |
| `--version` / `-V` | bool | clap built-in. |
| `--help` / `-h` | bool | clap built-in. |

### 2.2 IPC subcommands — stable

Selector convention: exactly one of `--name` / `--id` / `--focused` per
command (clap `conflicts_with_all`). When no selector is given, the default is
`--focused`.

| Command | Args | Maps to IPC |
|---|---|---|
| `renga list` | — | `Request::List` |
| `renga send` | `--name\|--id\|--focused`, `--enter`, `<TEXT>` (positional) | `Request::Send { append_enter }` |
| `renga focus` | `--name\|--id` | `Request::Focus` |
| `renga close` | `--name\|--id` | `Request::Close` |
| `renga new-tab` | `--command`, `--id`, `--label`, `--role`, `--cwd` | `Request::NewTab` |
| `renga split` | `--target-name\|--target-id\|--target-focused`, `--direction <vertical\|horizontal>`, `--command`, `--id`, `--role`, `--cwd` | `Request::Split` |
| `renga inspect` | `--name\|--id\|--focused`, `--lines`, `--cursor` | `Request::Inspect` |
| `renga events` | `--timeout <humantime::Duration>`, `--count <usize>` | `Request::Subscribe` + stream |
| `renga rename` | `--name\|--id\|--focused`, `--to-name`/`--clear-name` (mutex), `--to-role`/`--clear-role` (mutex) | `Request::SetPaneIdentity` |
| `renga mcp-peer` | — | (not IPC) handed off to `mcp_peer::run` for the stdio MCP loop |
| `renga mcp install` | `--client <claude\|codex>` (default `claude`), `--force`, `--codex-auto-approve-peer-tools` | (writes Claude/Codex MCP config; not an IPC call) |
| `renga mcp uninstall` | `--client <claude\|codex>` | (config write) |
| `renga mcp status` | `--client <claude\|codex>` | (config read) |

**`renga rename` (Q6)**: same semantics as `set_pane_identity` (§1.14) —
three-state via `--to-X` / `--clear-X` flags. Frozen in v1.0.

**`renga events` vs `poll_events` (Q2)**: see §1.15. The CLI form streams a
connection (good for shell pipelines and `tail -F`-style tooling); the MCP
form cursor-paginates (good for cooperative pull from peer agents). Both are
frozen.

### 2.3 Environment variables — stable (Q7)

These were de-facto stable; v1.0 makes them part of the formal contract.

| Var | Direction | Purpose |
|---|---|---|
| `RENGA_SOCKET` | published by parent renga, read by children | Path to the IPC endpoint (Unix socket on Unix; Named Pipe path on Windows). |
| `RENGA_TOKEN` | published by parent, read by children | Per-instance session token. Not a secret (same-user trust model); used as a PID-reuse defense. |
| `RENGA_PANE_ID` | published per-PTY by renga, read by `renga mcp-peer` | Numeric pane id the MCP subprocess belongs to. Absent → MCP runs in **detached mode**. |
| `RENGA_PEER_CLIENT_KIND` | injected by `renga mcp install --client codex` into Codex's MCP subprocess env | `"claude"` or `"codex"`. Defaults to `claude`. Selects the receive mode (`push` vs `pull`). |
| `RENGA_LAYOUTS_DIR` | read by CLI | Override layout search root. |
| `RENGA_NO_MACOS_TIP` | read by `macos_tip` | Set non-empty → suppress macOS first-launch banner. macOS-only. |

The historical `CCMUX_NO_MACOS_TIP` from the 0.10.0 release notes is **not**
part of the v1.0 contract; renamed to `RENGA_NO_MACOS_TIP` in the 0.18.x
sweep.

---

## 3. IPC protocol (Unix socket / Named Pipe)

Local-only, same-user, newline-delimited JSON. Not an authentication or
secrecy boundary — same-user processes are inside the trust boundary.

### 3.1 Endpoint naming — stable

- **Unix**: `$XDG_RUNTIME_DIR/renga/renga-<pid>.sock`. Fallback:
  `$TMPDIR/renga-<uid>/renga-<pid>.sock`, then `/tmp/renga-<uid>/renga-<pid>.sock`.
  Parent dir is forced to `0o700`. `<uid>` is the **real** OS uid (`getuid()`).
- **Windows**: `\\.\pipe\renga-<pid>` (Named Pipe). Default session-scoped ACL.

### 3.2 Connection lifecycle — stable

Short-lived per request:

1. Client opens connection, sends `Hello { client_pid }`.
2. Server replies `Response::Hello { server_pid, session_token }`. Client
   verifies `session_token == $RENGA_TOKEN`; mismatch → reject (PID-reuse
   defense).
3. Client sends exactly one `Request`.
4. Server replies one `Response`.
5. Server closes its side.

Exception: `Request::Subscribe` switches the connection to event-stream mode —
server replies `Response::Subscribed`, then emits `Event` JSON Lines until the
client disconnects. No further `Request`s are accepted on that connection.

Server budgets: 5 s `APP_REPLY_TIMEOUT` (server → app event loop) +
5 s `CLIENT_MARGIN` → 10 s `RESPONSE_TIMEOUT` from the client's perspective.

### 3.3 Request envelope — stable

`#[serde(tag = "cmd", rename_all = "snake_case")]`.

| Variant | Fields | Notes |
|---|---|---|
| `hello` | `client_pid: u32` | Required first message. |
| `list` | — | |
| `send` | `target: PaneRef`, `data: string`, `append_enter: bool` (default false) | |
| `split` | `target: PaneRef`, `direction: vertical\|horizontal`, `command?`, `id?`, `role?`, `cwd?` | |
| `focus` | `target: PaneRef` | |
| `close` | `target: PaneRef` | |
| `new_tab` | `command?`, `id?`, `label?`, `role?`, `cwd?` | |
| `subscribe` | `pane_id?: usize` | Switches to event-stream mode after ack. Bundled MCP peers identify their pane; generic consumers omit the additively introduced key. |
| `inspect` | `target: PaneRef`, `lines?`, `include_cursor: bool` (default false) | |
| `peer_list` | `from_pane: usize` | |
| `peer_send` | `from_pane: usize`, `target: PaneRef`, `body: string` | Unresolved and cross-tab targets return the same success-shaped `undeliverable` result and queue no body (Q5). Ok data is `{ "delivery": "delivered"\|"queued"\|"pending_user_confirmation"\|"undeliverable" }`. |
| `peer_register_client` | `pane_id: usize`, `kind: claude\|codex` | Posted by `renga mcp-peer` on startup. |
| `peer_set_ready` | `pane_id: usize`, `kind: claude\|codex`, `ready: bool` | Internal peer lifecycle update. `ready=true` means the event subscriber can receive; for push clients this is sent only after MCP initialization. Kind is repeated atomically with readiness. |
| `peer_inbox_ack` | `pane_id: usize`, `delivery_id: u64` | Internal mcp-peer receipt sent only after the target process retains the message. Separate from the later `check_messages` ack. |
| `set_pane_identity` | `target: PaneRef`, `name?`, `role?` (three-state: missing / null / value) | Uses serde `double_option`. |
| `set_summary` | `from_pane: usize`, `summary: string` | Empty `summary` clears. >256 `chars` rejected with `summary_too_long`. |

`PaneRef` = `{ id: usize } | { name: string } | "focused"`.
For `peer_send`, consistent with other tab-scoped pane operations, the
reference is resolved only within `from_pane`'s tab; `focused` means the
focused pane in that sender tab, even when it is inactive.

### 3.4 Response envelope — stable

`#[serde(tag = "status", rename_all = "snake_case")]`.

| Variant | Fields | When |
|---|---|---|
| `ok` | `data: Value` (request-specific shape) | Most success paths. |
| `hello` | `server_pid: u32`, `session_token: string` | Reply to `Hello` only. |
| `subscribed` | — | Ack of `Subscribe`; event lines follow on same connection. |
| `err` | `message: string`, `code?: string` | Failure. `code` is `Option<String>` with `skip_serializing_if = "Option::is_none"`. |

Request-specific `ok.data` shapes include
`split: { "id": usize, "startup_command"?: string | null }` and
`new_tab: { "id": usize, "startup_command"?: string | null }` and
`peer_send: { "delivery": "delivered" | "queued" | "pending_user_confirmation" | "undeliverable" }`;
lifecycle setters such
as `peer_register_client` and `peer_set_ready` return `null`. For `split` and
`new_tab`, a string is the effective command queued by the server, explicit
`null` confirms that no startup command was requested, and a missing key means
the server may predate effective-command reporting.

`PaneInfo` payload (used by `list` data, `set_pane_identity` ok data, embedded
in `peer_list` data):
`{ id, name?, role?, focused, x, y, width, height, cwd?, kind?, receive_mode?, summary? }`.

`PeerInfo` = `PaneInfo` minus the focused flag and geometry (purposefully
hidden from cross-pane callers).

### 3.5 Event envelope — stable

`#[serde(tag = "type", rename_all = "snake_case")]`.

| Variant | Fields | Notes |
|---|---|---|
| `pane_started` | `id`, `name?`, `role?`, `ts_ms` | One per pane creation. |
| `pane_exited` | `id`, `name?`, `role?`, `ts_ms` | Exactly once per pane id. |
| `events_dropped` | `count: u64`, `ts_ms` | Synthesized when a slow subscriber missed events. Per-subscriber. |
| `heartbeat` | `ts_ms` | Periodic; only purpose is to detect half-closed connections. Buffer cap 256/subscriber. |
| `peer_inbox` | `delivery_id?: u64`, `target_pane: usize`, `from_pane: usize`, `from_name?`, `from_kind?`, `body`, `ts_ms` | Always intra-tab by construction. Emitted at send time for a ready target, or when its subscriber becomes ready for a queued target. `ts_ms` remains the original send time. Bundled peers retain and deduplicate an additive `delivery_id`, then acknowledge it. Subscribers filter on `target_pane`. |

**`heartbeat` audience (Q10)**: emitted into the subscribe-stream
(`renga events` / `Request::Subscribe`). The MCP-side `poll_events` consumes
heartbeats internally as a half-close detector and does **not** surface them
to callers. v1.0 freezes this asymmetry.

**Forward-compat rule**: clients **must** ignore unknown `type` tags rather
than abort the stream. New variants are additive (see §5).

---

## 4. Layout / config files

### 4.1 `~/.config/renga/config.toml` — stable

(Windows: `%APPDATA%/renga/config.toml`.)

```toml
[ime]
mode = "hotkey"           # "hotkey" (default) | "off"
freeze_panes_on_overlay = true   # default true
overlay_catchup_ms = 3000        # default 3000; non-zero clamped >= 100; 0 = pure freeze

[ui]
lang = "auto"             # "auto" (default) | "ja" | "en"; case-insensitive
```

Missing or malformed file → warning to stderr, defaults apply (never fails
startup). Extra keys are ignored — additive forward-compat.

### 4.2 Layout TOML — stable (`version = 1`)

Search order: `$RENGA_LAYOUTS_DIR/<NAME>.toml` → `./renga-layouts/<NAME>.toml`
→ `~/.config/renga/layouts/<NAME>.toml`.

Top-level:

```toml
version = 1                # SUPPORTED_VERSION = 1; mismatch fails parse
name = "my-layout"         # non-empty
[root]
type = "pane" | "split"
# ... node schema below
```

Node — `type = "pane"`:

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Unique within layout. `[A-Za-z0-9_-]` only; non-empty. |
| `command` | string | no | Run after shell ready. |
| `role` | string | no | Free-form, may repeat. |
| `cwd` | string | no | Abs or relative-to-CLI-invocation. Falls back to parent pane's cwd; root leaf falls back to renga server cwd. |

Node — `type = "split"`:

| Field | Type | Required | Notes |
|---|---|---|---|
| `direction` | `"vertical"` \| `"horizontal"` | yes | |
| `ratio` | f32 | yes | Range `0.1..=0.9`; finite; otherwise reject. |
| `first` | node | yes | Recursive. |
| `second` | node | yes | Recursive. |

Caps: total pane count ≤ 16 (`MAX_PANES`).

The `version` integer is precisely the contract — any breaking schema change
ships as `version = 2` and the v1 parser continues to accept v1 files. The
parser already rejects unknown versions.

---

## 5. Errors, codes, and forward-compat

### 5.1 Error code catalog (`renga::ipc::err_code`) — stable

Wire ABI per the module's "Stability" doc-comment. The MCP layer surfaces
these as `[<code>] <human message>` in JSON-RPC error message strings.

| Code | Where | Meaning |
|---|---|---|
| `shutting_down` | every request | Server is shutting down. |
| `app_timeout` | every request | App event loop didn't reply within budget. |
| `parse` | every request | Request JSON failed to parse. |
| `protocol` | every request | Protocol violation (wrong message at wrong time). |
| `internal` | every request | Server invariant violation. |
| `pane_not_found` | pane-targeted requests | `PaneRef` did not resolve. Exception: `peer_send` returns `delivery=undeliverable` instead, so unresolved and cross-tab targets remain indistinguishable. |
| `pane_vanished` | pane-targeted requests | Resolved then disappeared mid-flight. Rare. |
| `split_refused` | `split`, `spawn_*`, `new_tab` (and layout TOML apply) | MAX_PANES = 16, or below `min_pane_width` / `min_pane_height`. |
| `io_error` | requests with PTY side-effects | OS-level write/spawn failure. |
| `last_pane` | `close` | Refused to remove the only pane of the only tab. |
| `cwd_invalid` | `split`, `new_tab` | `cwd` missing or not a directory. Pre-mutation rejection — no half-mutated layout. |
| `name_in_use` | `split`, `new_tab`, `set_pane_identity` | Another pane in the same tab holds the requested name. |
| `name_invalid` | `split`, `new_tab`, `set_pane_identity` | Name empty / all-digits / non-`[A-Za-z0-9_-]`. |
| `summary_too_long` | `set_summary` | Summary input exceeds 256 Unicode scalar values. Pre-mutation rejection. |
| `peer_queue_full` | `peer_send` | Target's pre-registration queue reached its message-count or byte cap. |
| `peer_delivery_unconfirmed` | `peer_send` | Target mcp-peer did not confirm local retention before the sender reply deadline. Safe to retry. |
| `codex_not_installed` | `spawn_codex_pane` | Codex's `~/.codex/config.toml` is missing the renga-peers entry, the file is unreadable, or the `RENGA_PEER_CLIENT_KIND=codex` env-var passthrough is absent. Surfaced from the MCP layer (not `renga::ipc::err_code`); branch on the `[code]` token same as the others. Run `renga mcp install --client codex` to remediate. |

### 5.2 JSON-RPC numeric codes (Q9)

- `-32602` invalid-params — input validation failures (empty `to_id`, unknown
  `send_keys` key name, conflicting `spawn_claude_pane.args` flag, unknown
  `inspect_pane.format`).
- `-32603` internal error — everything else, **including** renga-side errors
  carrying a `[code]` token. By design.

v1.0 does not split `-32603` further. Future minor releases may move specific
classes into more specific numeric codes; this is **not** a breaking change
because downstream is required to read the `[code]` token for branching.

### 5.3 Forward-compat rules — stable

- **Unknown event `type` tags**: ignore, do not abort the stream.
- **Unknown JSON keys** in config/layout/IPC payloads: ignored on read.
- **Missing JSON keys** have three contract-defined meanings: keys documented
  as optional-when-unset (for example `PaneInfo.name` and `Response::Err.code`)
  are unset when absent; keys with a documented default (for example
  `send.append_enter`, `inspect.include_cursor`, and `PaneInfo` geometry) take
  that default when absent; additively introduced keys that an older peer may
  predate (for example `split.startup_command`, `new_tab.startup_command`,
  `subscribe.pane_id`, `peer_send.delivery`, and `peer_inbox.delivery_id`) are
  unknown / unverified when absent and must not be interpreted as `false`,
  `null`, or confirmation that no action occurred. If a key qualifies as both
  optional-when-unset and additively introduced, use the conservative latter
  interpretation and treat its absence as unknown.
- **Unknown `peer_send.delivery` values** are unverified outcomes. The
  additively introduced `pending_user_confirmation` and `undeliverable` values
  therefore degrade to the documented unconfirmed result on older MCP clients;
  neither may be interpreted as `delivered` or `queued`.
- The `check_messages` input keys `max_response_bytes`, `message_id`,
  `offset_bytes`, and `ack`, plus result keys `delivery`, `pending_after`,
  `has_more`, `ack_required`, `acknowledged_message_id`, and all fields nested
  under `delivery`, are
  **additively introduced**. Their absence is unknown and must not be read as
  no-more, no-ack-needed, zero pending messages, or confirmed receipt.
- Adding `acknowledged_message_id` is additive: older clients ignore it, while
  newer clients can associate a confirmation with the removed FIFO head. By
  contrast, omitting `delivery` and the next body from a successful ack response
  is an intentional observable behavior change. Field evidence showed that LLM
  callers treated an ack result as confirmation context and did not process a
  chained body as a new coworker request; the following nudge and fresh
  `check_messages({})` call now provide that distinct request context.
- **Unknown `[code]` tokens**: treat as the equivalent of `internal`.

These rules let renga add fields and variants additively without bumping the
major version.

---

## 6. Global rules and out-of-scope

### 6.1 Global rules (apply across all surfaces)

- **All-digit `name` ↔ id rule**: a string consisting entirely of digits is
  always interpreted as a numeric pane id. `set_pane_identity` and layout TOML
  reject all-digit names; pane lookup interprets all-digit `target` strings as
  ids. This is a global lookup invariant.
- **`PaneRef::Focused` defaulting**: CLI subcommands default to `--focused`
  when no selector is given. MCP tools that accept a `target` default to
  `"focused"` only where documented (`spawn_*`, `set_pane_identity`); other
  tools require an explicit `target`.
- **Tab scoping (Q4)**: `list_panes`, `focus_pane`, `inspect_pane`,
  `send_keys`, `set_pane_identity`, and `close_pane` are
  **scoped to the current tab**. Panes on other tabs are not addressable in
  v1.0. A `peer_send` target (`send_message` on the MCP side) is scoped to the
  sender's tab, including when that tab is inactive.
- **Unresolved and cross-tab `peer_send` targets share one result (Q5)**:
  `delivery=undeliverable` raises no error, queues no body, and does not reveal
  whether a matching pane exists in another tab.
- **Detached-mode ok-text fallbacks**: `list_peers` and `send_message` return
  the documented ok-text prefixes (§1.1, §1.2) instead of JSON-RPC errors when
  the renga IPC server is unreachable. The prefixes are part of the wire ABI.

### 6.2 Out of scope for v1.0

The following are **not** part of the v1.0 frozen surface. They may exist in
the codebase but downstream must not depend on them; they may change in any
minor release.

- **Cross-tab selectors** for `list_panes` / `focus_pane` / `send_message`
  etc. (Q4 → v1.1+). Workers needing cross-tab coordination must continue to
  use the "all workers in one tab" pattern.
- **`spawn_pane.command` opt-out flag** for the `claude → claude
  --dangerously-load-...` rewrite (Q3). Callers who need verbatim execution
  must use a non-`claude` leading token (e.g. `bash -c '…'`).
- **Per-call `min_pane_width` / `min_pane_height`** on `spawn_pane` /
  `spawn_claude_pane` / `spawn_codex_pane`. Process-global only.
- **`peer_*` IPC variant naming as a stable surface**. The peer-routing
  subset of `Request` is reachable from downstream only via the MCP layer
  (§1) and the CLI (§2); the Rust-level variant names are not promised.
- **`SetPaneIdentity` `double_option` Rust encoding**. Wire JSON behavior
  (omit / `null` / value) is frozen; the Rust serde helper is not.
- **JSON-RPC `-32603` granularity** (Q9). Branch on `[code]`, not on the
  numeric.
- **`heartbeat` event in `poll_events`** (Q10). Internally consumed; not
  surfaced to MCP callers.
- **`CCMUX_*` legacy environment variables**. Retired in the rename sweep;
  not part of v1.0.

---

## Appendix — surface count

| Section | Count |
|---|---|
| MCP tools (§1) | 15 |
| CLI top-level flags (§2.1) | 11 |
| CLI IPC subcommands (§2.2) | 13 |
| Env vars (§2.3) | 6 |
| IPC `Request` variants (§3.3) | 16 |
| IPC `Response` variants (§3.4) | 4 |
| IPC `Event` variants (§3.5) | 5 |
| Error codes (§5.1) | 16 |
| Config schema sections (§4.1) | 2 |
| Layout TOML node types (§4.2) | 2 |
