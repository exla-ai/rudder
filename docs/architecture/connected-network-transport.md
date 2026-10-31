# Connected network transport

This is an explicit qualification transport, not managed connected execution.
The existing network profile 1 bytes, Spec4, Binding3 and offline handshake are
unchanged. No native default, provider launch, profile projection, checkpoint or
acceptance consumer is enabled by this module. A future managed integration may
propose Spec5/Binding4; those versions are not allocated here.

The first transport accepts IPv4 HTTPS CONNECT on a loopback listener created
inside an owned guest NET. Its host supervisor accepts that transferred socket
without entering the namespace. Separately created upstream sockets remain in
the supervisor's original NET. It creates no packet route, TUN device, host
listener, transparent proxy, privileged helper service or network namespace
transition in a parent thread.

## Ownership and admission

`network::connected::ListenerAdmission::receive` takes an actual unreaped
`std::process::Child`, an exact frame and three owned descriptors. The frame is
data, not authority. The admission path opens a pidfd while the Child remains
owned and checks liveness before and after inspecting `/proc/<pid>/ns/*`.

The descriptors are exactly child USER, child NET and listener. Namespace FDs
pass `fstatfs(NSFS_MAGIC)` before any namespace ioctl. The NET's actual owning
USER must equal the held USER. Both must equal the actual live Child's
namespaces and differ from the supervisor's. The helper must have all real,
effective, saved and filesystem UID/GID1500, no supplementary groups, NNP1 and
one thread. The full profile digest, every owner/attempt identity, readiness
nonce and listener port must match the validated profile.

The actual socket must be a listening, unconnected AF_INET/TCP stream bound to
127.0.0.1 and the reported nonzero port. It is nonblocking and CLOEXEC. Its
`SIOCGSKNS` FD passes the same NSFS/type/owner checks and must equal the held
child NET. The kernel socket cookie is an additional consistency check.
`SIOCGSKNS` requires CAP_NET_ADMIN in the USER owning that NET: an ancestor
relationship is not assumed to grant it. EPERM is unsupported; there is no
cookie-only fallback. The standalone qualifier runs supervisor and fresh child
as UID1500 so the actual child USER ownership rule can be tested.

`HostNetworkAnchor` retains the supervisor's actual thread NET. Every upstream
creation verifies `/proc/thread-self/ns/net`, and its socket cookie must equal a
socket created in that same NET. Resolver process namespaces are independently
checked through their owned Child/pidfd. No dial uses an accepted guest socket.

`ConnectAuthority` is an actual nonserializable host-held callback. It validates
the exact full binding against current owner/attempt authority and returns a
fresh trusted host IPv4 deny set. A cancel flag, saved profile or matching UUID
is never a grant. Production must include authenticated public/NAT host
addresses as well as interface addresses; the standalone synthetic authority
only inventories its local interfaces and reports that limitation.

`OwnedConnectProxy::start` retains startup ownership. The caller must receive
`await_ready` success before sending a child admission ACK. A fixed authority
worker refreshes a 500ms CLOCK_BOOTTIME validity bound. Connections fail closed
when it expires. Authority is also checked around resolution and dialing.
Every successful callback revalidates all bounded sealed origin pins against
the fresh host deny set before renewing validity. If any pin becomes host/NAT
denied, the whole proxy cancels and shuts existing sockets, including tunnels
already forwarding bytes. An unused fallback pin can conservatively fence the
proxy too; a refreshed inventory is never discarded merely because a dial
already succeeded.
Blocking callbacks occupy their fixed workers until actual return; timeout does
not release or replace them.

## Bounded effects

Profile 1 already bounds concurrent connections (1..8), resolver processes
(1..4), header bytes, combined per-connection buffer bytes, transferred bytes
and connect/idle/total deadlines. This transport additionally caps lifetime
accepted connections at 32, including malformed and zero-body requests.

There are fixed connection workers and fixed resolver worker processes with
bounded queues. Queued and active DNS jobs share the same max_dns_jobs permits;
a requester timing out cannot release its still-queued or working job's permit.
No per-request thread, async task or DNS thread is spawned.
Each resolver pins the actual loaded ELF through a held `/proc/self/exe` FD;
it never reopens a replaceable executable pathname. Exec resolves that held FD
and closes it through CLOEXEC before helper code. It runs an explicit mode, with
cleared environment, private standard pipes, no inherited authority FDs, NNP1,
parent-death SIGKILL and a 256MiB address-space bound. Requests and responses
are length-delimited, at most 4096 bytes, and carry an exact serial.

The resolver uses an absolute canonical name and IPv4/TCP `getaddrinfo`. Every
returned A address must be public, in the sealed origin pin set, outside the
fresh host deny set and within the four-address bound. A mixed private/public,
unlisted or oversized result rejects the entire request. There is no DNS
search-list expansion, IPv6 fallback or connect-by-name. Numeric dial attempts
share the original connection deadline and revalidate both grant and actual
connected peer.

A resolver deadline signals the actual owned pidfd. Its slot remains occupied
until the helper replies or is actually killed and reaped. It is not replaced
while a getaddrinfo call remains alive. Close timeout therefore yields an error
and retains the unsettled owner; it cannot manufacture a settlement receipt.

The parser accepts one bounded ASCII `CONNECT canonical.host:443 HTTP/1.1`
request, an optional exact Host and a small benign header allowlist. It rejects
other methods, credentials, literals, bodies, transfer encodings, duplicate or
folded headers, extra requests and already-buffered tunnel bytes. No client
header is forwarded to the origin. A 200 response is sent only after a checked
numeric connection exists.

One combined allocation holds each request and then its two relay buffers.
Short writes and EAGAIN preserve bounded pending data. Byte reservations are
atomic across workers; consumed headers, proxy response and both tunnel
directions remain charged. Unused reservations are returned after nonblocking
I/O. Half-close drains pending bytes before SHUT_WR and allows the reverse
direction until EOF, idle/total deadline, revocation or cancellation.

Cancellation shuts down registered sockets while their owners still hold the
exact FDs. Removal and shutdown share a mutex, so a recycled FD cannot be
signaled. Socket close precedes connection permit release. The accepted socket
cookie must equal the admitted listener's NET cookie.

`close_and_settle` cancels admission and I/O, joins the actual accept, authority,
connection and resolver workers, verifies zero active jobs and registered
sockets, and closes the retained guest namespace handles. Only then does it
return opaque `ConnectedSettlement`. Its audit snapshot can be serialized;
the settlement cannot. It proves endpoint settlement, not stopped PID trees,
RPC/FUSE drain, data ACK or a durable checkpoint.

## Qualification and remaining integration

The standalone `rudder-connected-network-qualifier` has no network effects by
default. `--describe` reports the explicit command. On a disposable owned VM:

```text
qualify --base ABS_PRIVATE_UID1500_DIR --host PUBLIC_FQDN --ipv4 SORTED_COMMA_PINS
```

The private base must already be canonical, owned by UID1500 and mode0700.
The tool creates a nonce-owned fixture beneath it. A fresh helper establishes
USER/NET using the existing offline primitives, creates the actual listener,
transfers USER/NET/listener with SCM_RIGHTS, closes its listener, and waits for
admission. It then drops all five capability sets, proves NNP1, closes namespace
handles and waits while the parent checks actual `/proc` state. The actual
Python process uses HTTPS_PROXY for a bounded HEAD / with normal public CA and
hostname validation. No key, model, provider account or personal cache is used.
The parent checks actual `/proc/<owned-pid>/net/dev` is lo-only before exec.
Failed fixtures retain an at-most-8192-byte mode0600 `failure.private.txt` with
bounded synthetic child diagnostics. Only its nonce basename is announced;
raw diagnostics never enter the public proof. Successful fixtures are removed.

This qualifies the transport, not Claude/Codex proxy support, provider auth,
private filesystem/PID/PTY isolation, managed readiness, VCS or checkpointing.
A CONNECT tunnel is opaque: fixed IP/port pins cannot enforce an inner TLS SNI,
HTTP Host or tenant behind a shared CDN. Stronger origin semantics require a
separate policy/profile, not a claim added to this receipt.

Next managed integration must preserve admission-before-guest, actual
SCM_RIGHTS/pidfd checks and namespace/reference teardown; include this endpoint
settlement in the real [workspace drain](execution-workspace.md); and keep
source-finalization authority alive while guest ingress closes. See
[managed networking](managed-network-isolation.md),
[remote composition](remote-managed-launch.md), and [PLAN](../../PLAN.md).
