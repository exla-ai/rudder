Native integration for n13

Register `pub mod terminal_stream;` in native/src/lib.rs. In the binary use
`rudder_native::terminal_stream::{View, Snapshot, Cursor, InputAuthority, Owner}`,
or add `mod terminal_stream;` to main.rs. Test with:

```
/workspace/rudder-build/rcargo test -p rudder-native --lib terminal_stream
```

These embedded tests are not discoverable until that registration is integrated.
No App, PTY, module registration, manifest or shared transport file was modified.

The App loop owns View and InputAuthority. Background socket readers send typed
messages with the connection token returned by View.reconnect(), captured when
the reader is created. Apply snapshots/deltas/exits only inside the App loop.
Compare the captured token again before applying asynchronous paint results.

Adapt each version-1 terminal-screen frame to Snapshot: cursor generation and
sequence are the frame's values; reset/gap are its explicit flags. Rows, cols,
complete ANSI screen and history come from the canonical screen snapshot, never
a ring-buffer tail. When the sender supplies typed cells, convert the complete
cell grid/styles/cursor/modes into a self-contained ANSI frame in the adapter.
View.snapshot always creates a fresh vt100 parser. Reconnect requires an explicit
reset snapshot. Out-of-order frames, missing sequences and exit before required
output request another snapshot; Receive::Stale drops obsolete callbacks.

The server's canonical parser owns incomplete UTF-8/escape/paste/synchronized
state. Complete screen snapshots serialize already parsed state; they must not
carry parser fragments. View.delta supports an optional contiguous byte path,
returning paint_ready=false while a codepoint/escape/synchronized frame remains
incomplete. Never recover by supplying a suffix of that byte path.

Before connecting InputAuthority to a native PTY, call replace with the current
connection generation and engine owner/run epochs. An authenticated control
message may grant the actual controller client ID. Controller promotion, owner
replacement, disconnect and exit revoke control; new connection/owner clears the
lease. Resize and raw input both require accepts with all three identities.
Call input immediately before the PTY write. It returns the original byte slice
and has no replay queue, so bracketed paste/key encoding is unchanged. Durable
privileged operations remain on their versioned receipt/idempotency path.

OutputQueue is per surface. On its overflow, discard the entire queue and request
a complete snapshot; reset_after_snapshot only after that snapshot is delivered.
Check socket buffered bytes before take(), with at most one bounded frame in
flight, and take queued output before enqueuing terminal exit/control. The App
loop should schedule the next drain at OutputQueue's latency budget rather than
sleeping or waiting in a rendering/input callback. Native PTY upstream queues
already retain their existing bounded reader/backpressure implementation.

View keeps received history separately as bounded plain text and exposes the
live vt100 screen. Preserve the existing TerminalPane styled history/selection
model in the App adapter. Snapshot acceptance changes rows/cols atomically with
the screen; do not silently resize the actual PTY from spectator geometry.

Trace timestamps are Instant offsets on one process clock. Correlation IDs join
input capture/send/relay/PTY/output/paint spans without comparing unrelated
clocks. Mark predictive local paint authoritative=false and worker echo true.
No local unit test qualifies laptop/Cloud latency or hardware release gates.

Concrete authoritative extraction is implemented by ScreenExporter. Construct
once per PTY/stream generation. After the existing vt100 parser processes every
original PTY byte chunk, call exporter.observe(chunk). Capture on the App loop
with exporter.capture(parser.screen_mut(), reset, gap): it returns
IncompleteFrame while UTF-8/escape/synchronized output is partial. It uses real
vt100::Screen::state_formatted(), temporarily selects history rows through
set_scrollback/rows(), and restores the exact saved viewport. The resulting
Snapshot.wire_screen() serializes the concrete `vt100-ansi-v1` payload with
columns, rows, screen and scrollback. Decimal-string conversion for the outer
wire generation is the adapter's job if the TS contract uses a string epoch.

Keep ScreenExporter across viewer reconnects. New viewers capture the existing
state, with reset=true/gap=true, without advancing the stream cursor unless the
parser changed. Capture one new state per fanout to live views. Call
mark_changed() after authoritative resize/non-byte parser state changes.
History extraction keeps newest complete lines within the total frame budget.
At EOF call capture_final(), deliver its complete reset/gap snapshot, then send
exit with that snapshot cursor. It captures the real final parsed screen and
discards only incomplete parser fragments that never formed screen content;
this also ends a synchronized frame without holding the last output forever.
