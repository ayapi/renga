# Pane synchronized output

## Finding

Renga's pane reader receives DEC private mode 2026 synchronized-output start
and end markers around some Codex redraws. Before this change, every PTY read
was passed immediately to the vt100 parser and followed by a `PtyOutput` event.
The render loop could therefore observe a synchronized frame after only some of
its reads had been applied.

A reader-side capture on Windows observed 67 of 67 synchronized frames with
paired start and end markers. ConPTY delivered about 8,270 bytes in 298 reads,
or roughly 28 bytes per read. Every captured synchronized frame spanned several
reads, with up to four reads for a 105-byte frame. Whether an intermediate
screen became visible depended on the approximately 25 fps render loop sampling
between those reads, rather than on only unusually large frames spanning reads.

The capture establishes that the markers reach renga's reader. It does not
establish whether Codex wrote those exact bytes or ConPTY produced them while
rewriting the stream. OpenAI Codex issues 39710, 10331, and 22953 provide the
upstream context for Codex redraw and synchronized-output behavior.

## Implementation

The reader recognizes `ESC [ ? 2026 h` and `ESC [ ? 2026 l` across read splits.
It continues running newline counting, OSC 7 cwd detection, window-title
detection, prompt latching, alternate-scroll detection, OSC 52 clipboard
handling, and test capture against each raw read exactly as before. Only the
vt100 parser update, mouse-protocol sampling derived from the parser, and
`PtyOutput` notification are deferred. The complete buffered frame is then
processed under one parser lock and reported with its total buffered byte
count.

The DEC 2026 marker bytes remain in the parser stream. vt100 0.16.2 ignores the
unsupported mode, so preserving the bytes avoids changing or delaying other
escape sequences that share a prefix.

Reader-side buffering was chosen over continuously mutating the parser while
freezing only the rendered snapshot. The parser is also read by peer composer
inspection, so freezing only rendering would still expose a partially applied
frame to automation. Buffering makes rendering and parser inspection observe
the same complete state.

Two safeguards limit a missing end marker:

- `SYNCHRONIZED_OUTPUT_TIMEOUT` is 350 ms. Because the PTY read is blocking, the
  deadline is checked on the next read; both normal EOF and read-error exits
  also flush the frame. A producer that opens a frame and goes silent leaves the
  last complete screen visible until it writes again or the PTY closes. This is
  a coherent stale screen while the producer is already stalled.
- `SYNCHRONIZED_OUTPUT_BYTE_CAP` is 1 MiB. Exceeding that buffered byte count
  flushes the accumulated bytes and returns the reader to pass-through mode.

Repeated start markers do not nest or reset the timeout. An end marker outside
a frame has no synchronization effect. All marker bytes still reach vt100.

## Measured scope

A later live Codex 0.154.0 capture through ConPTY found that synchronized
frames covered 8-21% of bytes across four captures. Every full-repaint marker
in those captures was outside frames, with zero exceptions; the v5 capture
contained 20 markers and the v2 capture contained 8. The four erase repaints
took 28.5-30.9 ms from `ESC[2J` and spanned 8-10 reads; their rewrites started
in the read after the erase. The approximately 1 KiB cursor-hide through
cursor-show rewrite arrived in one read in 18 of 18 cases, while the erase read
arrived separately.

Replay of the saved capture through vt100, one captured read at a time, counted
intermediate screens that differed from both the pre-burst and post-burst
screen:

| kind | burst reads | before equals after | third states |
|---|---:|:---:|---:|
| ERASE | 10 | yes | 5 of 9 |
| ERASE | 8 | yes | 4 of 7 |
| ERASE | 10 | yes | 5 of 9 |
| ERASE | 9 | yes | 5 of 8 |
| H-only | 8 | no | 0 of 7 |
| H-only | 5 | yes | 0 of 4 |
| H-only | 8 | yes | 0 of 7 |
| H-only | 5 | yes | 0 of 4 |

Erase repaints were transiently wrong in 4 of 4 cases. Measured `ESC[H`-only
repaints never exposed a third state, including the one that changed content,
so `ESC[H` alone remains per-read and does not open a hold.

## Erase-keyed hold

Outside an open DEC 2026 frame, `ESC[2J` or `ESC[3J` moves parser output into a
shared pane hold. The reader continues every raw-read detector unchanged, but
the parser and `PtyOutput` notification wait. The hold closes on the first
later PTY read or UI iteration at or after 40 ms, when the existing 1 MiB byte
cap is exceeded, or on either reader exit path. The UI scans every pane before
draining PTY events even when no pane is dirty, so an idle Codex pane cannot
remain frozen on its pre-erase screen. Its generated `PtyOutput` uses the full
released byte count, preserving frame diagnostics and active-tab dirty gating.

The UI polling interval is additional scheduling time, not part of the 40 ms
age check. At the default 30 fps it can add about 33 ms; at `--fps 1` it can add
up to one second. A later read also performs the age check before consuming its
bytes, preventing a second erase after the cap from being folded into the
first repaint.

DEC 2026 framing and erase holds do not nest. Erases inside a frame remain
frame data. Complete frames inside an erase hold remain held through their end
marker. If an unmatched frame begin exists when the erase hold closes, bytes
before that begin are released and the suffix remains an open synchronized
frame, preserving its parser atomicity.

A burst stretched past 40 ms by host load is cut mid-rewrite and shows the
same half-applied paint, rarer. An additional erase arriving inside the first
hold's 40 ms is likewise governed by that original fixed cap. The measured
scope is erase repaints and idle Codex; this investigation does not claim that
the field-observed sweep is gone before the separate live field check.

## Field capture and replay

The working-Codex field symptom is transcript rows flowing from top to bottom.
The earlier idle measurements do not establish its cause. Every pane now keeps
a bounded in-memory capture ring. This steady state creates no capture files.
Press `Alt+Shift+C` immediately after seeing the symptom, or call the
`dump_pane_capture` peer tool, to write a manual dump beneath the platform data
directory (`%LOCALAPPDATA%\renga\pane-captures` on Windows). Manual directories
are named `manual-<TUI-pid>-<token>` and are never removed silently.
The tool requires one explicit selector: `target` for one pane, or `all: true`
for every pane in the caller's tab.

Every manual attempt writes `outcome.json` first and finalizes it after the
per-pane workers finish. It records the start/finish state, requester, selected
pane ids, and each pane's status, reason, paths, retained counts, and time
range. Panes run concurrently with a fresh three-second allowance each. A pane
that cannot finish the full snapshot in time writes its newest replayable
suffix, marks it `partial`, and returns both paths; one slow pane does not spend
another pane's allowance. The optional peer trace records the same lifecycle,
but `outcome.json` is authoritative for ordinary launches without peer tracing.

An erase hold that reaches its time/byte cap before the replay classifier finds
any printable rewrite payload also requests an automatic dump. A cap release
after rewrite bytes have arrived is the ordinary repaint case and does not
trigger. A forced ring cut is the other automatic trigger, limited to raw-byte
starvation past the hard ceiling when chained holds provide no safe cut.
Ordinary structural-record eviction is not a forced cut and does not write a
file. An admitted automatic trigger waits 10 seconds before taking its snapshot
so the following rewrite is present; another trigger during that delay does not
restart it. `outcome.json` records the original trigger time and reason. Each
pane has its own limiter, driven by the capture's monotonic elapsed clock: at
most one dump is admitted per pane per 10 minutes. Suppressions are reported in
dump metadata as `automatic_dump_suppressions`.

Automatic directories are named `auto-<TUI-pid>-<session-token>-<pane>-<n>` and
contain a renga ownership marker. Cleanup considers only directories with both
that exact numeric name and marker; it never removes manual directories,
unmarked directories, or foreign files. By default the oldest automatic dumps
are removed first to retain at most 32 dumps and 128 MiB total. Configure these
limits with `[debug].pane_capture_auto_dumps` and
`[debug].pane_capture_auto_total_bytes`; zero disables automatic persistence.
With the default 4 MiB raw ring and the forced-cut slack, the raw-byte write
rate can reach about 30 MiB per pane-hour, or 390 MiB/hour for 13 panes, plus
JSONL records. The retained automatic data still cannot exceed the configured
128 MiB total after a writer completes pruning.

For scripted continuous capture, set the legacy environment variable before
starting the TUI:

```powershell
$env:RENGA_DEBUG_PANE_CAPTURE = 'C:\scratch\renga-pane-capture'
renga
Remove-Item Env:RENGA_DEBUG_PANE_CAPTURE
```

On Unix, use `RENGA_DEBUG_PANE_CAPTURE=/tmp/renga-pane-capture renga`.
The variable is read once on TUI startup and enables only continuous disk
output; it is not required for the in-memory ring, manual dumps, or automatic
signals. Without the variable, startup and ordinary recording perform no file
system activity.

Each TUI creates a unique `session-<TUI-pid>-<start-token>` subdirectory with
`pane-<id>.bin` and `pane-<id>.jsonl`. Files use exclusive creation, so multiple
TUIs and restarted sessions cannot interleave their bytes. `.bin` contains
every positive-length raw PTY read in arrival order, without interpretation;
it includes the pane's transcript and any application output. Preserve both
files when sharing a capture. Capture records data; it changes no hold policy.

Run replay from the matching source checkout:

```powershell
$env:RENGA_DEBUG_CODEX_PEER_LOG = "$PWD\target\replay-test-peer.jsonl"
$env:RENGA_PANE_REPLAY = 'C:\scratch\renga-pane-capture\session-123-456\pane-1.jsonl'
cargo test --bin renga pane_capture::replay::replay_capture -- --ignored --exact --nocapture
```

`RENGA_PANE_REPLAY_GAP_US` optionally changes the default **40,000 us** burst
gap. Keep the default when comparing to the swk/1fz measurements. Replay needs
metadata and read records; parser application, draw, and tick-release records
are optional for converted historical captures. A metadata/read/parser-apply
capture prints its raw and applied tables with no drawn samples and envelope
`draws=n/a`.

### File schemas

Manual and automatic snapshots use compact schema version 2. The first JSONL
line remains a self-describing metadata object; event lines are arrays whose
first item is a type code (`r` read, `t` transition, `a` parser apply, `k` tick
release, `d` draw, `z` resize, `x` reader exit, or `g` gap). Per-read records
remain individual so timing and burst analysis are unchanged. Common pane,
process, origin, and retention data live in metadata instead of being repeated
on every event. The replay helper accepts both versions.

Continuous capture retains schema version 1 for compatibility. Each UTF-8
JSONL line is an object. Common fields are `sequence` (zero-based,
contiguous per pane), `timestamp_unix_ms`, `elapsed_us`, `pane_id`, `process_id`
(the **TUI** process), `child_process_id` (PTY child or null), and `event`.
The process-wide origin is sampled once; Unix timestamps are that origin's
Unix milliseconds plus the event's monotonic elapsed time. They do not jump
when the wall clock is adjusted. Read timestamps use the exact instant passed
to the output stream; tick releases use the App's tick instant; actual draws
sample their instant under the parser lock. The writer never samples event
timestamps. Tests can inject both the origin and reader clock.

**File/channel sequence is authoritative. Do not sort by time.** A tick can
sample its instant before waiting behind a reader, so its timestamp can be
earlier than a preceding record. The parser's `applied_offset` changes under
the same parser lock as `parser.process`, and actual draws read that offset
under their existing parser lock. It is the exclusive end of the raw-byte
prefix already applied to vt100, starting at zero.

| Event | Additional fields and meaning |
|---|---|
| `metadata` | First record: `version: 1`, initial `rows`, `cols`, `origin_unix_ms`. |
| `read` | `bin_offset` (zero-based start), positive `read_len`, `deferred` after this read. Consecutive read ranges cover the binary stream. |
| `transition` | `action`: `marker`, `open`, `close`, or `promote`; `kind`: `marker`, `dec2026`, or `erase_hold`; `marker`: `dec2026_begin`, `dec2026_end`, `erase_display`, `erase_scrollback`, or null; `bin_offset`; `reason` or null. Marker/open offsets include marker prefixes split across reads. Close offsets identify the consumed position at the release decision. |
| `parser_apply` | `bin_offset`, `byte_len`, `applied_offset`. Applies that raw range, which starts at the previous applied offset. Ranges can end partway through a read or combine several reads. |
| `app_tick_release` | `released_len`, resulting `deferred`; the actual App-side erase release, including its own tick timestamp. |
| `app_draw` | `drawn`, `applied_offset`, `scrollback`, `deferred`, and optional `repeat` / `last_elapsed_us`. True means terminal content was copied into this App frame; false means this pane was skipped. Equal consecutive draws are coalesced before both ring and continuous fanout; `repeat` preserves their logical count. A draw is an App buffer render, not confirmation of host-terminal presentation. |
| `resize` | `rows`, `cols`, `clear: true`, `applied_offset`. At this parser position, call `set_size`, then process `ESC[2J ESC[H` (with no intervening space). These injected bytes are not raw PTY bytes and do not advance the applied offset. |
| `reader_exit` | Resulting `deferred`; emitted after applying the reader's final released bytes, for EOF or a read error. |
| `gap` | `bin_offset`, `missing_raw_bytes`, `dropped_records`. A producer or continuous-writer queue loss ends the current burst and resets replay parsers; tables never span it. Replay also derives equivalent gaps from missing read offsets in legacy captures. |

`deferred` is `{ "kind": "none|dec2026|erase_hold",
"opened_at_elapsed_us": null|integer, "buffered_len": integer }`.
Draws carry the latest completed reader/tick deferred snapshot; the applied
offset identifies the parser state actually copied. A frame inside an erase
hold gets its own open/end-marker close records. An unmatched frame that
survives the erase release has action `promote`, reason `erase_conversion`;
its original begin instant remains in the subsequent deferred state.
Close reasons are `end_marker`, `timeout` (reader-side age check), `byte_cap`,
`reader_exit`, and `tick_release`. Open reason `inside_erase_hold` identifies
a frame whose bytes are still retained by an erase hold.

For example, the following minimal converter output corresponds to binary
bytes `abc`. The replay helper accepts these common fields without requiring
the optional child PID:

```jsonl
{"sequence":0,"timestamp_unix_ms":1000,"elapsed_us":0,"pane_id":1,"process_id":123,"event":"metadata","version":1,"rows":40,"cols":120,"origin_unix_ms":1000}
{"sequence":1,"timestamp_unix_ms":1001,"elapsed_us":1000,"pane_id":1,"process_id":123,"event":"read","bin_offset":0,"read_len":3,"deferred":{"kind":"none","opened_at_elapsed_us":null,"buffered_len":0}}
{"sequence":2,"timestamp_unix_ms":1001,"elapsed_us":1000,"pane_id":1,"process_id":123,"event":"parser_apply","bin_offset":0,"byte_len":3,"applied_offset":3}
```

Replay validates sequences, dimensions, and read/apply ranges. It reports
missing read and apply ranges separately, including apply records that overlap
unavailable input, instead of rejecting the rest of a damaged legacy capture.
An unindexed
binary tail is reported and ignored, allowing analysis of complete records
from a capture interrupted between binary and JSON writes. A malformed or
partial JSONL line is an error; preserve the original and remove only its
incomplete final line from an analysis copy if a process was forcibly killed.

### Reading the replay tables

A burst is a maximal run of consecutive reads with each internal gap **less
than 40,000 us**. Each row reports reads, bytes, duration, largest internal
gap, and a byte-based kind: `ERASE` if it contains `ESC[2J` or `ESC[3J`, otherwise
`H-OPEN` if it contains `ESC[2m ESC[H` (without a space), otherwise `other`.
A bare `ESC[H` does not count as an H-OPEN repaint.

Screen comparisons use vt100 `contents()` text, with each row trimmed at the
end and blank rows retained to the screen height. The burst's `pre` is the raw
screen before its first read; `post` is the raw screen after its last read.
Raw replay pre-indexes resize injections and inserts them at their recorded
`applied_offset` in byte order. A resize recorded while bytes are held must
precede those held bytes in raw replay, even though their read records have
already arrived. Actual applied/drawn replay retains file order and performs
the resize at its record, matching the live parser. This prevents a delayed
resize clear from manufacturing a false raw post-screen or third state.
A resize exactly at a read's exclusive end is applied before the next read,
after the preceding read's post-screen has been sampled. It must not turn a
completed burst into a cleared screen just because the resize record follows it.
Each table prints `before_equals_after` and `third_states=X/Y`: a third state
differs from **both** pre and post. `raw` samples after reads 1 through n-1,
reproducing the original "5 of 9" form; `applied` samples each actual parser
application ending within the burst's byte range; `drawn` samples actual pane
draws from its first read record until the next burst's first read record.
A draw with nonzero scrollback is labeled `scrolled_view` and excluded from
third-state samples and partial-envelope/rewrite draw counts. It does not
replace the previous live-view screen used for the next live draw comparison.
After a resize clear, live draws are labeled `resize_clear` until the next
positive-length parser application. These draws, and any zero-byte application
snapshots in that interval, are excluded from repaint third-state and partial
envelope/rewrite counts. Burst rows separately report `resize_clears`,
`resize_clear_draws`, and `resize_clear_applies` over their record window. The
black frames remain visible in the report without being attributed to PTY
rewriting. Ordinary repaint sampling resumes when PTY bytes are applied.
A release triggered by a later read is therefore still assigned to the bytes
it applied. Delayed or suppressed draws can produce fewer samples.

Each draw and table snapshot also reports its change from the previous screen:

- `identical (none)`: all rows match; this includes identical rewrites.
- `cleared (full clear)`: the later screen is entirely blank.
- `shifted(k) matches=m (scrolled)`: for k from 1 through rows-1, maximize
  the number of nonblank later rows equal to earlier row i+k. At least three
  rows must match and this count must exceed the unchanged nonblank count.
  Ties choose the smallest k. A fixed footer does not exclude a shifted body.
- `rows_changed(n)`: other changes, with the number of differing rows.
- `scrolled_view scrollback=n`: a user-scrolled view; excluded from repaint
  comparisons as described above.
- `resize_clear`: a live view after a parser-injected clear, before another
  PTY byte is applied; counted separately from repaint intermediates.

Every third state prints mutually exclusive `post_only`, `pre_only`, `both`,
and `neither` row counts. `top_prefix=true` means post-only rows precede
pre-only rows, with both sets present; rows equal to both are neutral. At most
one neither row may sit between those sets if it is a partial overwrite:
for some character count `0 < k <= len(post[j])`, it equals
`post[j][..k] + pre[j][k..]` (an exhausted old suffix is empty). For example,
writing `del` over `charlie` towards `delta` leaves `delrlie`; writing all of
`delta` before the line erase leaves `deltaie`, which also qualifies. The table prints
its zero-based `partial_row` and `partial_chars=k`, or `none` for both when
the split falls between rows. Matching uses Unicode characters, not bytes.
Any other neither row makes the flag false. The mutually exclusive row counts
still count the partial row in `neither`; this measures the shape separately.

Every cursor envelope beginning in a burst, from `ESC[?25l` through the next
`ESC[?25h`, reports its half-open byte span, completion, first/last read indexes
(one-based across the capture), read count, whether it spans multiple reads,
and duration. `draws` counts true draw records whose applied offset is strictly
inside that span; these are the draws that could expose a partial envelope.
No extra record is made for every idle App iteration. An incomplete envelope
ends at the last indexed byte and prints `complete=false`. Burst rows also
count DECSTBM, SU, SD, IL, DL, RI, EL (`ESC[K`), and ED (`ESC[J`), including
numeric CSI parameters, to identify scrolling operations.

The additional `erase_rewrite` table follows each raw erase cluster into its
later rewrite, even when a long gap places it in a separate 40 ms burst.
Consecutive erase commands with no printable payload between them form one
cluster: `ESC[2J ESC[3J` (without the space) produces one row. Its rewrite search
starts after the final command, avoiding a false early-close row for the first
erase. `erase_commands` and `cluster_span` describe the group. An intervening
resize clear ends the group and its attribution.
`kind=PTY_ERASE` rows name the erase's burst, byte offset, recognition read,
and timestamp using the first command in the cluster; a split first command
is recognized by the read containing its final byte. `first_rewrite_delay_us`
is the time from that read to the read containing
the first printable payload byte after the erase. Cursor positioning, style,
cursor visibility, and OSC/DCS/control strings are skipped when finding that
payload; spaces count as payload. Thus an erase followed only by a home command
does not report a zero-delay rewrite.

`hold_close_reason` comes from the corresponding erase-hold transition.
`closed_before_rewrite=true` requires both an earlier close record and a
strictly earlier close instant than the first payload read. A reader-side
expiry with the arriving read's timestamp is therefore false. Missing hold
evidence prints `unknown`; if the capture has a close but no later payload,
the flag is true for the observed capture and the delay is `n/a`.
`rewrite_span` starts at the first payload byte and ends with that payload's
40 ms burst, limited by the next raw erase or resize clear. `rewrite_bytes`
counts the raw span, including its intervening/trailing controls;
`rewrite_reads` counts reads intersecting it. `draws_inside_rewrite` counts
true draws whose applied offset is strictly inside the span, before any next
resize clear and with scrollback zero; it is `n/a` without draw records. This association is an explicit
measurement rule, not proof that arbitrary later text belongs to a repaint.
The accepted attribution rule deliberately stops at a resize clear: the raw
erase is not credited with subsequent rewrite reads after that separate clear.
For the reviewed A1 trace this yields `rewrite_reads=1`; the resize has its own
row rather than being silently folded into the earlier erase episode.

Parser-injected resize clears appear as separate `kind=RESIZE_CLEAR` rows,
with their sequence, timestamp, applied offset, and seven injected clear/home
bytes. Their hold fields are `not_applicable`: they bypass the raw stream and
do not open an erase hold. This distinguishes a resize blank from an erase-only
hold released before a delayed multi-read rewrite.

These text classes diagnose parser visibility. They do not compare colors,
cursor-only changes, host-terminal flush completion, or the physical screen.
Draw scrollback offsets and parser resize clear injections are replayed.

### Cost, errors, and flushing

The creator resolves capture and passes the handle into the reader's shared
stream. Under the stream lock the reader enqueues raw bytes and typed records;
parser applications additionally hold the existing parser lock. Draw/resize
records use that parser lock and a short capture-state lock; they never
acquire the stream lock. The pending-draw and dropped-event locks are leaf
locks: code holding either one never acquires a parser or stream lock, so taking
them while the parser lock is held cannot form a lock cycle. Serialization and
file IO happen only in the per-pane
writer thread, using two buffered writers. Consecutive equal draws are
coalesced on the producer side, before ring and continuous-disk fanout. The
producer backlog, structural ring records, and raw ring bytes are independently
charged; whole read-led groups are evicted in constant time instead of
rescanning the ring on every event. Queue overflow is represented by a gap
rather than silently corrupting replay.

Both files flush at fixed 100 ms deadlines even under continuous traffic,
on close/release (including the resulting parser application), reader exit,
and writer disconnect. A TUI-return guard requests acknowledged flushes with
one shared deadline of at most one second. It includes writers for panes that
already closed; capture handles/files remain registered until TUI shutdown.
Forced termination cannot run that guard. Open, spawn, write, or flush failure
increments an atomic failure counter and disables that capture; senders check
disabled before copying raw data. There is no diagnostic stdout/stderr output
inside the TUI and pane parsing continues unchanged.

The default per-pane in-memory ceiling is approximately 13.004 MiB: 4 MiB raw
ring, at most 1 MiB forced-cut slack plus one 4096-byte read, 4 MiB structural
record budget, and 4 MiB producer backlog, plus fixed bookkeeping. The ring
uses 40-byte compact records and one preallocated raw-byte allocation; vector,
group, and deque overhead is charged to the structural budget. Snapshots share
sealed record groups and copy the contiguous raw ring once. The acceptance
load is 6 pane-24 streams at 10x plus 6 pane-28 streams at 30x in a maximized
window. Its no-capture 5e402df reference was 156--160 MB private memory and
0.061 CPU cores; b5b4dfd reached 1,363 MB at 606 seconds, averaged 11.3 cores,
and lost every pane in an all-pane dump. The corrected build is checked for 30
minutes against `<= 160 MB + 12 * 13.004 MiB` private memory, `<= 0.5` total
capture CPU core, and a final 12-pane dump containing `outcome.json` plus either
complete or path-bearing partial results.

The release ring microbenchmark measured 0.152 us/push in its first
full-capacity window and 0.130 us/push in its last (0.86x; 40-byte compact
records and 144-byte transient queue records). A corrected three-minute
preflight on the same 12-pane rig reached 287 MB at 181 seconds and 29
CPU-seconds total (about 0.160 core, or 0.099 above the no-capture reference).
Its all-pane dump completed 12/12 in 184 ms with a written outcome and both
paths for every pane; aggregate JSON/raw size was 3.165x. It created zero
automatic directories, confirming structural eviction has no disk side effect.
The final 30-minute run remains the acceptance measurement for long-run
flatness.

The preserved field dump's original retained interval was exactly 35m58.792s
(5,471,262,171 through 7,630,054,047 us). Replaying that pane-24 record shape
through the corrected 4 MiB structural ring retained the newest 223.194 seconds,
778,037 raw bytes, and 58,658 records. Time retention is workload-dependent;
the configured caps, rather than a promised duration, define the guarantee.
