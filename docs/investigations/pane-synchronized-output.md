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
frames covered only 8-21% of bytes. All 20 full-repaint markers across four
captures were outside frames. The four erase repaints took 28.5-30.9 ms from
`ESC[2J` and spanned 8-10 reads; their rewrites started in the read after the
erase. The approximately 1 KiB cursor-hide through cursor-show rewrite arrived
in one read in 18 of 18 cases, while the erase read arrived separately.

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
