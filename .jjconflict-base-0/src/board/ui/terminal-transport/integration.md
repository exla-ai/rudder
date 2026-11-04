# Integration handoff for n13 and E13 operator

These modules implement local fault-tested mechanisms. Shared call sites are
intentionally unchanged because n13 owns their wiring. This is not evidence of
an integrated or deployed Cloud terminal/private-preview feature.

## Terminal sender and Cloud relay

`cloud/src/private-access/terminal-session.ts` exports TerminalScreenSession,
TerminalSocket, TerminalScreen, TerminalCursor and ScreenFrame. No new package
is needed. Construct one session per authenticated project/workspace PTY after
existing account and project ACL checks, using the current owner/run identity
as its opaque generation. An address or Tailscale device never grants ownership.

In cloud/src/server.ts, extend Channel with an optional screen session. Negotiate
an explicit version-1 screen capability on both worker and client attachments;
keep the legacy binary attach contract for old clients. Never feed legacy ring
buffer bytes into a new screen snapshot. Attach/detach each current socket using
the returned TerminalView identity, and capture that object in all message,
close, error and send callbacks. Pass the existing authenticated client id, not
an untrusted arbitrary controller claim. TerminalScreenSession retains the first
controller, replaces a returning id with a fresh connection lease, and promotes
a live spectator after controller disconnect. It never carries old input over.
Handle read-only recovery control before server.ts's current early
`channel.controller !== ws` return, otherwise spectators cannot acknowledge
paint or request their own snapshots. For example, inside the negotiated branch:

```ts
const view = channel.screen.attach(clientId, ws, resumeCursor);
// Capture view in this socket's callbacks; do not look it up by clientId later.
ws.on("message", (data, binary) => {
  if (!binary) {
    const message = parseBoundedVersionOneControl(data);
    if (message?.type === "terminal-painted") {
      channel.screen.acknowledge(view, message); return;
    }
    if (message?.type === "terminal-snapshot-request") {
      channel.screen.requestSnapshot(view, message.cursor); return;
    }
  }
  // Existing authenticated input/resize dispatch calls screen.input/resize;
  // no legacy pushPendingInput or latestResize replay in this negotiated path.
});
ws.on("close", () => channel.screen.detach(view));
```

`parseBoundedVersionOneControl` names the n13-owned boundary adapter, not an
export supplied here: parse under the existing socket payload cap, require
version===1 and the documented type, select only string generation and safe
integer sequence, and never forward arbitrary client JSON as worker control.
Intercept worker canonical screen/exit frames before forwardTextToClients; their
source must match the authenticated current worker and owner/run generation.

TerminalSocket uses send(data, callback), the ws text-frame signature. Route only
current canonical `vt100-ansi-v1` state through publish(screen). It coalesces to
one newest screen, default 4 ms, with at most one unpainted frame per viewer.
Default bounds: 2 MiB UTF-8 total screen and history, 10,000 history rows, 32 views,
256 KiB socket high water and 5 s stalled viewer retirement. JSON escaping can
expand a frame by at most 6x; the socket/serializer bound must include that plus
its small envelope. A slow observer neither blocks a fast observer nor pauses
all PTYs. Route a version-1 `terminal-painted` control message carrying
{generation,sequence} to acknowledge(view,cursor), using the captured current
view lease. A ws send callback alone never releases this slot. Wrong/stale paint
acknowledgments neither release the slot nor renew its timeout. Route an
explicit per-view snapshot request to requestSnapshot(view,cursor); this does
not publish another project's screen or reset another viewer. Publish each
parsed final state before exit(code); the exit envelope
waits behind the final per-view snapshot. Reconnect always sends a complete
reset snapshot; cursor divergence/skipped intermediate output sets gap=true.

Current controller input calls input(view, currentGeneration, bytes, correlation)
immediately. Forward accepted input with its captured owner generation to the
existing worker writer and recheck its engine owner/run epochs immediately
before PTY write. This callback is not a scheduling or authorization bypass.
No input is queued/retried if the worker is absent; no old Ctrl+C/paste/action
is promoted into another owner. Controller geometry calls resize with the same
lease; spectator geometry cannot resize the PTY. `replaceOwner` removes every
old view and decoder. Durable privileged operations keep existing idempotency,
epoch and receipt reconciliation instead of using terminal byte input.

A native authoritative producer is implemented, not a raw byte stand-in: see
native/src/terminal_stream/integration.md. `ScreenExporter` observes the same
chunks as the existing vt100 parser, retains partial UTF-8/escape/synchronized
state, and captures Screen::state_formatted() plus bounded history without
changing the saved viewport. Keep this producer across viewer reconnects. The
producer generation is a numeric stream id; the authenticated adapter maps it
to the existing owner/run identity and does not truncate an engine epoch through
a JavaScript number. TerminalScreenSession assigns its own per-session delivery
sequence; forward Snapshot.wire_screen(), not a competing outer cursor.

For worker-side node-pty output in cloud/worker/{home,supervisor}.mjs, the current
createTerminalTransport remains the bounded byte path. Negotiate canonical
screen production from the native source before enabling the screen relay.
A worker emitting only binary tails cannot claim screen recovery support.

## Browser/native receiving and paint ordering

`src/board/ui/terminal-transport/recovery.ts` exports TerminalRecovery and
terminalLatencyBounds. Construct recovery with the existing terminal renderer.
`connect(authenticatedGeneration, controller)` returns a connection token. Capture
it in every socket callback. `accept(token, frame)` validates/fences a screen;
`acceptExit(token, frame)` defers exit until final paint acknowledgment.

`restore(screen, paint)` must replace the complete emulator/display model,
geometry and its bounded scrollback, including cursor/modes from screen.screen.
Do not append it to an old escape parser. Build the replacement off-thread if
necessary, then check `paint.isCurrent()` immediately before the UI/App-loop
mutation and invoke `paint.painted()` only after that exact frame is painted.
Use a per-generation emulator when asynchronous parser writes cannot be canceled.
Restore an existing selection/scroll anchor through the pane's current model;
never apply spectator geometry as a controller resize. Late paint callbacks
cannot acknowledge output for a replacement connection or older frame.
The required acknowledge(cursor) adapter sends
`{version:1,type:"terminal-painted",...cursor}` after fenced actual paint.

Input uses input(token, bytes, correlationId) and has no replay buffer. Explicit
controller status uses setController. Preserve existing key/Option encoding,
bracketed paste, prediction and echo reconciliation. Log predictionPainted and
predictionRolledBack separately; authoritative-paint is emitted after actual
paint acknowledgment. Optional legacyOutput has a fresh streaming TextDecoder
per connection; never mix it into a negotiated full-screen attachment.

Native module registration and actual App-loop message adapters are n13-owned.
Add `pub mod terminal_stream;` to native/src/lib.rs and consume its public module
from the binary. Then run:

```
/workspace/rudder-build/rcargo test -p rudder-native --lib terminal_stream
```

The 14 native tests are present but not runnable through Cargo before registration.
No Rust pass is claimed by this worker. Existing worker byte-transport tests and
the new TS screen tests are separate evidence; the headless parser fixture tests
text/cursor/history only and does not qualify production styled screen wiring.

## Preview integration

`src/private-access/previews.ts` exports PrivatePreviewDirectory,
observeLinuxPreviewListeners, previewProcessMatches, processStartToken,
loopbackPreviewOrigin and executeTailnetCommand. Feed only engine-registered
preview-role processes with tenant, project, workspace, attempt, exact workspace
path, PID and start token. Linux discovery verifies cwd containment and socket
inode ownership, then HTTP readiness; it does not scan all listeners by port.
Use reconcile on a bounded asynchronous discovery loop and retire attempt state
when its process exits. Same-device ambiguous ports fail closed.

Existing cloud/worker/home.mjs hooks: scanPorts currently deduplicates by port
before ownership and emits only {port,label,url,https}; replace that discovery
with complete references and publish ready metadata into existing project state.
Retain the private Serve owned-port journal through writeJson/updateJson; do not
persist ready observations as proof on boot. Timeout of a Serve effect is unknown
and blocks reassignment until reconciliation. Do not enable Funnel or public
listeners. Persisted tailscaled Running identity skips fresh enrollment/up;
bounded status/probe/Serve checks are asynchronous and kill hung child commands.
The directory verifies the actual Serve root Proxy target and rechecks the
process's exact socket inode/port/origin before opening. A repointed service is
withdrawn and preserved. A numeric-only persisted port journal cannot prove
target ownership after restart, so existing routes are quarantined rather than
overwritten or removed. Persist the full route target identity if safe automatic
restart retirement is needed. CLI total deadlines also close inherited output
pipes, so a forked descendant cannot keep discovery waiting indefinitely.

`cloud/src/private-access/preview-access.ts` exports AuthorizedPreviewReports.
At the existing home worker state receive path (server.ts home control message),
pass reportingScope from the authenticated worker/session record. Call receive,
then list/resolve only after tenant/project ACL checks. Call retire on worker or
attempt loss. It stamps this relay process's monotonic receipt time, rather than
comparing a remote monotonic timestamp. Keep unknown/incomplete refs out of the
preview-open action; the old cloudPreviews API is legacy display compatibility.

CLI cloud.ts preview open must resolve the exact preview id through authenticated
Cloud state, verify local persisted status device ID and bounded HTTP/TLS route
readiness, and call its existing local openBrowser with only the resolved URL.
PrivatePreviewDirectory.open supplies this logic when invoked beside its live
process observations; do not deserialize a remote directory as a local one.
For Cloud-to-laptop opening, use AuthorizedPreviewReports.resolve on the relay
and the local previewAccess/previewReachable helpers; re-resolve after probes
before opening so revocation/stale attempts cannot launch. A TLS relay URL must
be minted by the existing authenticated relay for this exact reference and
retain tenant authorization. No public fallback or upload/redeploy is implied.

## Real laptop/Cloud E13 protocol for n0/coordinator

Only the designated Fly qualification owner runs remote steps, staging only.
Do not alter protected home machine 8edd225a7d0378 or rudder-workers-exla config.
Keep provider secrets in its environment/private stores; redact artifacts and
perform the documented staging cleanup and token revocation. This worker ran no
Fly/provider or real-tailnet commands.

Record revision, UTC times, OS/kernel, versions, region, transport and negotiated
screen capability. On an authorized laptop and disposable Cloud workspace:

1. Boot with an already enrolled persisted Tailscale identity and no new key.
   Record Running/backend, stable Self.ID, online state and an actual private
   HTTP/TLS result. Repeat with stopped daemon, expired login, transient startup,
   ACL deny and hung CLI. Discovery/UI/PTY input must remain responsive and
   offline references must not become ready or launch a browser.
2. Start identifiable IPv4 and IPv6 dev servers in two projects/attempts, with
   known content and process start tokens. Include identical ports on distinct
   devices and an ambiguous same-device port. Include an unrelated/control
   listener. Open from the laptop through the existing authenticated action;
   verify exact project content/device/attempt and tenant denial for another
   actor. Exercise HTTPS enabled/disabled policy and outside-tailnet TLS relay.
3. Exit/restart/reassign each process, revoke project access during a delayed
   probe and reconnect the laptop after sleep. Verify old refs stop opening,
   routes are removed/reconciled, new attempts get new refs and no Funnel or
   public service appears. Inspect actual Serve state and managed journal.
4. On both home and sail/workspace terminal paths, use a controller and slow
   spectator across multiple active projects with continuous output. Inject
   split Unicode, CSI/OSC, bracketed paste and synchronized frames. Drop sockets
   mid-sequence and while input/Ctrl+C is pending. Replace owner/run. Verify
   reset/gap plus complete actual screen/history/cursor/modes, fresh decoders,
   no stale callback or input write, preserved geometry/selection and final
   output before exit. Check the negotiated canonical snapshot source, not an
   arbitrary tail or a test-generated text repaint.
5. Collect >=1,000 samples per scenario: key capture to local predictive paint,
   authoritative echo/paint and rollback; large paste; scroll/full repaint;
   first attach and sleep reconnect. Retain raw monotonic events with correlation
   ids, queue/socket/snapshot byte bounds, RSS over sustained slow viewing and
   counts of snapshots/gaps/disconnects. Report count/p95/p99/max separately per
   scenario and clock domain. Across hosts report synchronized-clock error bounds
   or endpoint-local RTT; do not subtract unrelated monotonic timestamps.
6. Record the raw results and any wiring/platform gaps. Local module tests,
   ignored/unregistered native tests, text-only headless fixtures and local
   integration are not E13 hardware, deployment or release proof.

## Workspace snapshot warning

Late `jj status` attempts fail with a sibling-operation error: the working copy
references operation ca1b48870d4a while the shared operation log is elsewhere.
`jj --ignore-working-copy diff --stat` reads an older snapshot and omits final
filesystem edits. This worker did not repair or mutate the shared operation log.
The coordinator must reconcile the operation state and snapshot the actual
workspace files before integrating. Do not treat that stale diff as the final
change set. No commits, branches, merges, pushes or deployments were created.
