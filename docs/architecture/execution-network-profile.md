# Execution network profile

Status: alpha.12 candidate, 2026-10-31. Strict configuration validation and
child-only Rust namespace/capability primitives are implemented; execution
wiring, egress, live network ownership and default activation remain proposed.
See [the ownership boundary](execution-workspace.md). This narrows the next
slice of [managed networking](managed-network-isolation.md).

## Measured privilege boundary

The [alpha.10 disposable observation](../../examples/execution/network-namespace/evidence/2026-10-30-alpha10-netns/README.md)
established UID1500 USER/NEWNET creation and in-process loopback setup on
`6.12.114-fly`. New USER creation reset effective/permitted/bounding capability
sets to `000001ffffffffff`, despite all five sets being zero at worker boot.
External exec cleared effective/permitted/inheritable/ambient sets but retained
that child-namespace bounding set. The original evidence wrapper's contrary
bounding-set assertion failed; its raw evidence and limitations remain explicit.
TUN open failed `EACCES`; packet-device creation was not qualified.

Current [drop_privileges](../../engine/src/execution/linux.rs) sets NNP and zeros
effective/permitted/inheritable through capset. It does not explicitly drop or
verify the bounding set. It makes no all-five-sets guarantee today. The new
profile must establish and verify that stronger boundary before untrusted exec,
after the final trusted USER/namespace/device setup. Boot state alone is
insufficient. This does not claim capabilities in the child user namespace
grant authority over its parent.

Required sequence in the single-threaded child: finish privileged setup; clear
ambient capabilities; drop every kernel-supported bounding capability while
effective CAP_SETPCAP is still available; capset effective/permitted/inheritable
to zero; set/verify NNP; read back all five sets as zero. Dropping CAP_SETPCAP
from the bounding set does not itself clear its effective bit, so capset must
come last. Capability enumeration is bounded and must reject an unsupported
kernel/capability ABI instead of silently ignoring higher bits or errors.
Verify in every backend/server/adapter exec branch on the actual executing
thread; failure withholds launch and credentials. See
[bounding-set drop](https://man7.org/linux/man-pages/man2/PR_CAPBSET_DROP.2const.html),
[ambient clear](https://man7.org/linux/man-pages/man2/PR_CAP_AMBIENT_CLEAR_ALL.2const.html)
and [capget/capset](https://man7.org/linux/man-pages/man2/capget.2.html).

The all-zero measurement is the final exec boundary. It does not assert that
NNP or an empty bounding set disables subsequent creation of a nested user
namespace. Network isolation must survive that operation: the guest receives
no host namespace FD, host AF_INET socket or route into the host network.

## Sealed request and live ownership

`NetworkProfileV1` validates strict bounded `format_version:1` configuration.
Its offline mode requires empty policy and zero egress budgets; the qualification
mode checks canonical policy, exact expected identities and a trusted host deny
inventory. It opens no connection and grants no current-attempt permission.
The next execution wiring embeds it inside an explicitly new spec version
greater than3. Allocate that
outer version together with remote composition; do not independently reserve4.
Existing v1-v3 canonical request bytes and unmanaged behavior stay unchanged.
Old helpers must reject the new version. Missing or invalid configuration for a
requested new profile is Unsupported before selected-key login or inference.

| Field | Proposed contract |
| --- | --- |
| `binding` | Exact current owner, run ID/epoch, request/execution/operation/view IDs and per-attempt network UUID; cross-check sealed spec, managed binding and broker binding |
| `mode` | `offline_v1` or `qualification_https_connect_v1`; no general/default packet mode |
| `policy` | Bounded canonical bytes with a SHA256 digest, explicit public HTTPS qualification origins/ports and IPv4-only destination policy; host recomputes digest |
| `budgets` | Positive host-capped connection/DNS-job counts, parser/buffer bounds, byte ceilings and connect/idle/total deadlines; checked arithmetic and no unbounded queues |
| `readiness_nonce` | Fresh one-use challenge for the existing private owned helper channel; no credential in argv, terminal input, public logs or persisted grants |

The profile contains instructions and identity, not live permission. Proposed
host types are `PendingNetwork`, `LiveNetwork` and `NetworkDrain`; their fields
and constructors are private and none is Deserialize. They hold actual owned
namespace descriptors, helper/backend pidfds, private control endpoints and
proxy cancellation/settlement guards. No public constructor accepts a boolean,
PID inventory claim, saved receipt or guest-authored identity as ownership.
Use the existing held EngineOwnership/managed handoff capability and current
attempt admission; introduce no competing owner lock or scheduler.

`PendingNetwork` becomes `LiveNetwork` only after matching actual kernel FD
witnesses, trusted setup/privilege evidence and exact owned process readiness.
Admission to backend authentication requires both that live guard and the
existing owned-backend readiness/authentication protocol. No disk file or
deserializable local deadline recreates the guard after restart. Historical
network receipts support audit/reconciliation only.

## Setup and shutdown order

1. Complete the one-use outer abstract Unix handoff. Keep the outer supervisor,
   FUSE, broker and remote source/cache on the host network. Do not add NEWNET to
   `supervisor()` or the generally used `PrivateMountNamespace` helper.
2. In fresh `run_child()`, after its own self-mapped USER/MNT entry and before
   the PID fork, unshare NEWNET. Open and hold actual user/net namespace FDs;
   check NS_GET_NSTYPE, compare device/inode against the held parent witness,
   and match NS_GET_USERNS to the newly created user namespace. Transfer only
   these typed witnesses over the existing authenticated private FD channel;
   never reopen a guest-supplied `/proc/<pid>/ns` path. NEWNET also isolates
   abstract Unix names. [Linux namespace semantics](https://man7.org/linux/man-pages/man7/network_namespaces.7.html)
3. Raise `lo` with in-process ioctl before exec/capability drop. Initial profile
   has no external interface, default route, guest DNS, TUN or host network
   fallback. Finish PID/root setup without introducing threads before raw fork.
4. After root restriction, start a trusted owned guest-loopback CONNECT adapter
   with a dedicated private Unix byte transport to the host policy proxy.
   Adapter/backend/server children receive the verified privilege boundary
   above. The untrusted backend never inherits namespace/control FDs, raw host
   network sockets or proxy administration. Keep the existing owned backend
   socket peer/PID checks and authenticated frontend barrier.
5. Match readiness to network UUID, full attempt binding and nonce using live
   FD/pidfd ownership. The host checks the actual witness types/ownership rather
   than trusting serialized inode numbers or `network_isolated:true`. Only then
   permit selected credentials and inference through existing admission.
6. Graceful stop withdraws guest network admission, closes that attempt's streams
   and kills/reaps its network helpers. Retain exact workspace/source finalization
   authority and its refresh monitor through PID/VCS/RPC/FUSE drain, retained-data
   ACK, any required transfer-owned clone ACK, execution-record fsync and matching
   authenticated shutdown receipt, following
   [remote finalization](remote-managed-launch.md). Actual owner/run revocation
   invalidates RemoteSession and withholds checkpoint proof. Backend exit,
   helper/proxy failure or an uncertain control channel also withdraws network
   admission; none grants finalization authority. Return network resources only
   after real DNS/socket/helper settlement. Cleanup uncertainty prevents
   checkpoint/slot reuse and requires explicit reconciliation. A timeout or Drop
   is not successful settlement.

## Host proxy and later packet profile

The host proxy enforces destination policy, even when guest tools ignore proxy
variables. Resolve each new destination, validate every candidate and connect
directly to the chosen numeric address. No unchecked resolver retry, redirect,
IPv6 fallback or helper bypass may replace the checked target. Bound actual DNS
workers and retain their admission until settlement. Policy changes or attempt
revocation invalidate existing work according to the bound lifetime.

The first profile allows only selected public HTTPS qualification origins on
port443 and rejects IPv6 entirely. General policy additionally needs explicit
host/public-interface-address, private/CGNAT/link-local/metadata/Fly6PN/ULA,
multicast/reserved, mapped-address, scope-ID and DNS-rebinding checks. Address
changes require a refreshed trusted host deny set or admission freeze. NAT64
requires more than denying ULA: translation can use network-specific prefixes
and `64:ff9b:1::/48`. Reject translation or validate its configured prefix and
embedded IPv4 destination; unknown translation policy fails closed.
[RFC6052](https://www.rfc-editor.org/rfc/rfc6052.html),
[RFC8215](https://www.rfc-editor.org/rfc/rfc8215.html).

| Transport | Remaining gate |
| --- | --- |
| CONNECT qualification | Actual allowed HTTPS bytes, raw bypass denial, bounded revocation/cleanup; tools that ignore the proxy fail closed |
| slirp4netns/libslirp packet path | Pinned companion/library, private TAP access, independent destination policy; disable host loopback and address the documented built-in DNS alias path |
| pasta packet path | Pinned companion, private TAP access, disable host/guest address mappings and automatic TCP/UDP forwarding in both directions; independently enforce destination policy |

Neither packet option is selected or qualified. See the upstream
[slirp4netns manual](https://github.com/rootless-containers/slirp4netns/blob/master/slirp4netns.1)
and [pasta manual](https://passt.top/builds/latest/web/passt.1.html). General Cloud
development still requires a qualified packet path for raw Node/native TCP,
Git SSH and arbitrary build tools. CONNECT qualification cannot become the
general/default environment by renaming its profile.

NEWNET hides guest listeners from host `/proc/net` discovery. Preserve previews
through an owned `PreviewRegistration {attempt, network, guest_port, lease}`:
trusted code verifies the actual listener and creates only its narrow HTTP
relay, with known-project attribution, private tailnet Serve and exact cleanup.
No automatic forwarding or guest-selected host ports; existing unmanaged
previews retain their behavior.

## Alpha.12 completion gates

Before any default activation, require deterministic binding/version/budget
tests, injected failure at every setup/privilege step, and an owned disposable
end-to-end qualification. Measure all five capability sets before and after
actual exec; verify parent namespace/status unchanged and owned cleanup.
Exercise nested USER creation without host reachability, direct raw bypass,
host/private/link-local/6PN/IPv6/NAT64 aliases, DNS rebinding, cross-attempt and
stale readiness, stalled DNS/streams, revoke, helper death and uncertain drain.
Then qualify real provider HTTPS, the selected general packet path and guest
previews separately. No existing evidence establishes those later gates.
