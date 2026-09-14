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

The same capture forced a full-pane clear and rewrite of about 2.7 KiB. That
large repaint was not enclosed by DEC 2026 markers. This change makes marked
synchronized-output frames atomic; large unmarked repaints remain applied per
read and are tracked separately.
