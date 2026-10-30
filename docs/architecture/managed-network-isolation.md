# Managed guest networking and board authority

Status: source review and proposed fix, 2026-10-30. No listener probes, live-home
access, credential access or exploit execution were performed. The findings
describe reachable code when normal startup succeeds, not observed deployment
state. The alpha.11 working tree implements the
[board capability fix](board-access.md); it is not yet released. Controlled
guest networking remains unimplemented. Managed/default activation requires the
network and board gates below, alongside [managed launch](managed-launch.md)
and [remote composition](remote-managed-launch.md).

An owned [alpha.10 Fly observation](../../examples/execution/network-namespace/evidence/2026-10-30-alpha10-netns/README.md)
now demonstrates UID1500 USER/NEWNET creation and in-process loopback setup on
kernel 6.12.114-fly. TUN open returns EACCES; no packet path or egress was tested.
The original wrapper failed an incorrect postcheck about bounding capabilities;
raw observations and the corrected offline audit remain separate. The child
user namespace resets the bounding set, so boot-time zero capabilities are not
the final exec boundary. The [proposed execution profile](execution-network-profile.md)
records the stronger privilege/readiness and networking gates for the next slice.

## Concrete source finding

Managed execution isolates files, users, mounts and processes but retains the
host network namespace. [PrivateMountNamespace](../../engine/src/vfs/linux.rs)
unshares USER/MNT; the execution child adds PID isolation, not NEWNET.
[ExecutionIsolation](../../engine/src/execution/linux.rs) explicitly reports
network_isolated=false. Therefore guest 127.0.0.1 reaches host loopback listeners.
Read-only mounts, a private HOME and closed authority FDs do not change that.

The pre-alpha.11 normal home startup chain is
[home.mjs Project.spawnDashboard](../../cloud/worker/home.mjs) -> plain rudder ->
[main.ts launch](../../src/main.ts) -> ensureBoardRunning(scheduler:false).
[daemon.ts](../../src/daemon.ts) starts a projector board; a standalone board can
instead own the scheduler. Startup is best effort, so this establishes an
exposure when that listener exists, not that every deployed home has one.

[The board server](../../src/board/daemon.ts) binds 127.0.0.1, normally port 4774.
Before the alpha.11 capability fix, the reviewed path behaved as follows:

1. GET /, /rudder, /rudder/:slug or /usage requires only a loopback Host header.
   renderShell embeds the per-daemon BOARD_TOKEN as window.__RUDDER_TOKEN__.
2. A native guest HTTP client controls Host, can omit Origin and can read that
   shell. CORS, DNS-rebinding checks and token entropy do not authenticate it.
3. GET /api/projects, /api/projects/:slug/state, /events and /tasks/:id/log are
   readable without the token. The board exposes registered project/peer state
   across attempts; machine-wide usage endpoints are also unauthenticated.
4. With the shell token, POST tasks, tasks/:id/steer, steer, nudge-all,
   tasks/:id/merge and tasks/:id/cancel passes denyMutation. ownerRepoRoot limits
   writes to that daemon's project but does not bind the caller to a run.
5. Projector requests write .rudder/steer; native
   [poll_steer_inbox](../../native/src/main.rs) consumes them as user controls.
   Scheduler mode additionally invokes host scheduler/jj paths. This is concrete
   cross-attempt data/control exposure, not proof of an arbitrary /exec endpoint
   or direct unauthenticated vault/CAS access.

Relevant source locations are board daemon lines 173, 233–365, 381–707 and 1633;
main.ts lines 924–948; home.mjs lines 243–255; native main.rs line 15145. These are
review anchors, not a stable wire-version definition.

## Other reviewed surfaces and existing controls

| Surface | Existing boundary |
| --- | --- |
| Worker/home supervisors | [supervisor.mjs](../../cloud/worker/supervisor.mjs) and home.mjs dial outbound bearer-authenticated WebSockets; neither defines an inbound TCP command server |
| Worker terminal and home control WS | [server.ts](../../cloud/src/server.ts) requires the matching worker bearer for /api/rudder/{sail,workspace}/:id/worker and /api/rudder/home/:id/control |
| Terminal attach and phone/home RPC | Attach requires a user bearer and matching account; /api/app begins with requireAccount and selects that account's home/repo |
| Vault export | /api/rudder/{sail,workspace}/:id/secrets requires the matching worker bearer; admin setup requires admin credentials/session |
| Fabric objects/roots/staging | [fabric-http.ts](../../cloud/src/fabric-http.ts) authenticates before dispatch; server.ts derives account from user bearer or workspace-bound worker bearer |
| Native execution/attach/broker/VFS/VCS/backend controls | Pathname Unix sockets, not TCP listeners; protected paths plus the scoped token/binding/current-authority checks apply to projected broker/VFS/VCS endpoints |
| Dev previews | [home scanner](../../cloud/worker/home.mjs) excludes Rudder/board processes; [tailscale-previews.mjs](../../cloud/worker/tailscale-previews.mjs) requires known project ownership, loopback HTTP readiness and private Serve, never Funnel |

No additional unauthenticated host-exec, vault or CAS network endpoint was found
in these sources. Cloud's loopback Host handling selects auth redirects; it does
not bypass the bearer/session checks above. Fly6PN reachability alone grants no
Cloud API authority. This review does not inventory installed third-party dev
servers, Fly init services or a live machine's listeners.

## First fix: explicit board capabilities

Protect the board independently of network isolation, preserving the user's
existing local browser/projector/scheduler behavior:

1. Remove capabilities from anonymous HTML. Static assets and a neutral shell may
   remain public; every project/usage/log/SSE read and every mutation requires a
   host-issued capability with explicit project/read/control scope. Retain Host,
   Origin, request-size and owner-project checks as additional controls.
2. Bootstrap the authorized browser from a host-only capability channel. For
   example, mint a short-lived bootstrap ticket through a private Unix endpoint
   and place it in the browser URL fragment; exchange it for the bounded board
   session, then erase the fragment. Anonymous HTTP cannot mint/recover tickets.
   Do not put credentials in shared context, ordinary child environment, public
   HTML or diagnostic URLs. Daemon restart/revocation invalidates old grants.
3. Update board ui/types.ts to authenticate all fetches, including reads and
   retries. ui/store.ts currently uses native EventSource, which cannot attach
   the proposed authorization header: use authenticated fetch streaming with
   bounded SSE parsing/reconnect, or a deliberately authenticated scoped cookie.
   Do not place reusable capabilities in SSE query parameters.
4. Update src/daemon.ts and src/main.ts startup, daemon reuse and standalone
   board open. Native main.rs currently receives RUDDER_BOARD_URL, reconstructs a
   fallback port and opens /web or /usage: replace secret bootstrap through common
   environment with a host-only handoff/private control lookup. Opening the
   browser must not leak a credential into agent environments or notices.
5. Preserve Cloud relay authentication. home.mjs currently queues native steer
   directly after authenticated home RPC; it does not proxy anonymous board
   HTTP. If control is unified behind the board, give the supervisor an explicit
   host capability for the selected project and preserve caller/account/run
   authorization, revocation and durable receipts. Update its dashboard startup
   and control callers rather than scraping shell tokens or trusting Fly6PN.
   Never place that capability in baseChildEnv or PTY input.

This patch closes the identified board bootstrap gap. It does not make arbitrary
host loopback services safe for a shared-network managed guest.

## Mandatory managed network boundary

The selected next slice should use a fresh guest network namespace with
controlled egress. Create NEWNET in the trusted execution child before guest
code/capability drop; keep the outer supervisor, broker and remote source/cache
on the host network. Compare actual namespace identities using owned descriptors
and authenticated readiness, not a guest-provided boolean or reusable PID.
Missing setup is Unsupported before credentials/inference, never host-network
fallback. Existing unmanaged launches retain their current behavior.

One narrow qualification transport is a host-owned per-attempt egress proxy, reached
through a projected authenticated Unix endpoint and a guest-loopback adapter.
The guest namespace has no unmediated external route. Proxy configuration in
the sealed spec binds owner/run/attempt, policy digest, endpoints and budgets;
the proxy enforces it even when a tool ignores HTTP_PROXY/HTTPS_PROXY. Such tools
fail rather than gaining raw host connectivity. This is not the final general
development profile: arbitrary Node/native TCP clients must work through a
qualified packet-networking path before that profile becomes the default.

The proxy resolves and validates the destination for each new connection, pins
the chosen address, and rejects host addresses, loopback, private/link-local,
Fly6PN/ULA and mapped-address aliases. Redirects require new policy checks;
DNS rebinding cannot replace the checked address. Public Internet access can be
the normal development policy; tighter provider/package allowlists are explicit
profiles. Private targets need scoped project grants, without per-request user
approval for already authorized endpoints. Bound jobs,
bytes, DNS work, streams and deadlines; revoke on the exact attempt's authority
loss, retaining capacity until real socket settlement. Proxy helpers and preview
relays join owned shutdown/cleanup; no background worker becomes a scheduler.

Rootless packet networking is an alternative implementation, not an exemption
from this policy. The current [pasta manual](https://passt.top/builds/latest/web/passt.1.html)
documents host gateway mapping and automatic forwarding; a candidate must disable
host mapping and automatic forwarding in both directions, including TCP/UDP,
and independently enforce private/host destination policy. The
[slirp4netns manual](https://github.com/rootless-containers/slirp4netns/blob/master/slirp4netns.1)
documents --disable-host-loopback but also a possible host-loopback path through
its built-in DNS alias. Neither a stock helper nor that single flag proves
isolation. Pin and qualify any selected image companion; none is selected here.

## Preserve real previews and qualify activation

The host /proc/net scanner will not discover listeners in a new guest netns.
Add an owned PreviewRegistration { attempt, network, guest_port, lease } path:
trusted helper verifies the actual attempt/listener, then creates a narrow host
relay for that HTTP port. Keep existing known-project ownership, HTTP readiness,
private tailnet Serve and exact owned cleanup. Never automatically forward all
guest ports or expose board/broker/backend/proxy administration. Unmanaged
preview discovery must continue to work. A guest claim cannot select a host port
or another attempt's namespace.

Required checks before managed/default activation:

| Case | Required result |
| --- | --- |
| Raw same-netns HTTP with loopback Host/no Origin | Anonymous shell contains no capability; project/log/usage/SSE/control calls denied |
| Authorized native/standalone browser, reuse and restart | Board/projector/scheduler/usage and SSE reconnect work; revoked grants fail |
| Guest to host loopback, gateway/DNS aliases, host IP and Fly6PN | No host control/data bytes; include IPv6/mapped forms and DNS rebinding |
| Two owned attempts and forged/stale namespace registration | No cross-attempt board, proxy or preview access |
| Selected provider HTTPS, ignored proxy settings, setup failure | Allowed traffic works; raw bypass and unsupported setup fail before inference |
| Proxy timeout, stalled transport, revoke and helper death | Bounded resources, no early admission release, exact owned cleanup |
| Real guest dev server and existing unmanaged preview | Correct private URL/bytes, no automatic forwarding and no control endpoints exposed |
| Cloud attach/home RPC and vault/Fabric negative auth | Legitimate account flows preserved; missing/wrong bearer denied independent of network origin |

First implement board capability regressions and the sealed network profile,
then qualify child namespace/proxy/preview composition on an owned disposable
worker. Source review alone does not satisfy those execution gates.
