# Local board access

The local board uses an explicit browser session. Loopback reachability, a Host
header and the public HTML shell do not grant access to project state, logs,
usage, event streams or controls. This is the alpha.11 implementation; release
and deployment evidence is recorded separately.

## Browser handoff

The Node owner exposes a private Unix endpoint and credential-free descriptor
under the explicit Rudder home. Their directory is owned by the current user
and mode 0700; files and sockets are private. Long socket paths use an owned,
private per-user directory under `/tmp`. Clients verify the canonical repository,
protocol, daemon instance and bound loopback origin. Symlinks, foreign owners,
nonprivate modes and nonregular descriptors are refused.

Opening the board asks that endpoint for a fixed destination: project, index or
usage. The host opener receives a random, one-use ticket in the URL fragment.
The browser erases the fragment before its first asynchronous request, exchanges
the ticket with the selected daemon and keeps the bearer in memory and
origin-scoped sessionStorage. Tickets expire after 30 seconds; sessions expire
after one hour and renew while the browser is active. Restarting the daemon
invalidates its old tickets and sessions.

The ticket and bearer never appear in anonymous HTML, normal child environment,
daemon PID files, TUI notices or browser-opener errors. Native browser actions
use a bounded background callback and typed result. The App loop applies that
result without replacing a pending confirmation or the person's input draft.

## Scope and streaming

A session may read registered projects and machine usage, and control only the
board owner's project. Existing Host, Origin, repository and scheduler/projector
checks still apply. This is a host-user capability, not an agent capability.
Agents use their separately authenticated, run-scoped peer/control protocol.

All private HTTP routes require the bearer, including reads and SSE. Authenticated
fetch streaming replaces EventSource. Frames, connections, concurrent streams,
queued output and reconnect delays are bounded. Expiry and daemon shutdown close
existing streams. No bearer is placed in an SSE query string or a port-shared
cookie. Anonymous clients receive a neutral shell with instructions to open the
board from Rudder; HTTP cannot mint a bootstrap ticket.

Cloud's authenticated home RPC remains a separate path. This change does not
make other host loopback services safe for a guest that shares host networking.
The [guest network isolation gate](managed-network-isolation.md) remains open.

## Source and checks

The authority and private handoff are in `src/board/authority.ts` and
`src/board/host-access.ts`; browser session/streaming code is in
`src/board/ui/access.ts` and `events.ts`. Native uses
`native/src/board_access.rs` and the App's browser-result polling.

Focused checks cover anonymous denial, one-use exchange, origin/instance binding,
cross-project control refusal, renewal/expiry, streamed delivery, corrupt private
endpoints and retained confirmation/draft state. A release must also run the real
TUI browser hotkeys and verify the bundled browser flow.

The [disposable Chromium qualification](../../examples/board/evidence/2026-10-30-capabilities/README.md)
passed bootstrap, authenticated rendering/SSE, reload, synthetic usage and
same-port restart. It used private staged bundles and synthetic provider logs.
Its source envelope predates a Rust test-only short-temp-path cleanup; browser
code was unchanged. It is not a Cloud, provider-login or guest-network proof.
