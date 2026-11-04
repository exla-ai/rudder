# Restricted Linux VFS execution

Status: the explicit Linux helper passed disposable Fly qualification on
2026-10-30. Default agent launch remains disabled pending managed view, engine
ownership and VCS integration. Native macOS retains its existing execution path.
This profile covers filesystem/PID isolation, not a network namespace or cgroup
resource limits.

## Trusted launch contract

The engine supplies `ExecutionSpec` after authenticating the attempt and owner
epoch. It includes owned VFS and execution roots, UUID operation/execution/view
identifiers, the assigned `OwnerEpoch`, guest command/environment, read-only
public runtime projections, narrow private home seeds, an optional broker
directory and a deadline. `authority_roots` explicitly names journal, auth and
peer identity roots. Public runtime projections cannot overlap those roots, the
CAS/VFS/execution roots, or the broker directory in either direction. `/` and
host home trees are never runtime sources. Guest runtime targets are restricted
to public runtime locations; writable projections are fixed to the view and
this execution's own home, scratch and temporary directories.

The helper receives no journal database or general host filesystem authority.
The caller is the trusted engine ingress; this is not an agent mount-request API.
Guest home seeds contain only this run's selected credentials. They are copied
to private `0600` files outside the source checkpoint. The JSON spec/environment
is never persisted to a spec file, CAS object, argv or logs. Receipts contain a
request digest and lifecycle metadata, not plaintext credentials.

## PTY and descriptor handoff

`portable-pty 0.9.0` forcibly closes every descriptor numbered three or higher
before exec. `ExecutionHandoff` therefore binds a one-use Linux abstract Unix
socket and keeps the configuration in a fully sealed memfd. The terminal command
contains only non-secret socket routing and the parent PID. After the real
`TerminalPane` owns the child, the engine activates handoff for that exact PID,
holding a pidfd. Kernel `SO_PEERCRED` checks exact UID/PID on both sides; the
helper also checks its actual parent and fences parent death. Only that child
receives the FD with `SCM_RIGHTS`, once, before a bounded deadline. The receiver
requires all write/grow/shrink/seal protections and a bounded regular memfd.

Standard input/output/error remain the real PTY. Lifecycle JSON is never
appended to terminal bytes. The protected execution record carries proof, and
the helper's final exit code reflects the agent's exit.

## Process and filesystem structure

The fresh UID/GID 1500 supervisor creates a self-mapped user namespace and
private mount namespace, opens its assigned VFS view and owns the FUSE server
outside the agent root. Worker boot must grant `/dev/fuse` access to group 1500;
the runtime installs no setuid helper or privileged broker.

Because FUSE has a server thread, the supervisor execs a fresh second helper
before creating a PID namespace. That wrapper creates another self-mapped
user/private-mount namespace and forks its PID 1 before starting threads. PID 1
constructs a private tmpfs root with selected recursive read-only runtime binds,
only its writable view/home/tmp/scratch, individual harmless devices, private
devpts and a read-only procfs exposing this PID namespace. Source inodes are
pinned before bind; recursive `mount_setattr` hardening is mandatory. There is
no permissive fallback when a kernel operation is unavailable.

`pivot_root` removes the old host root. The root tmpfs is read-only. Agent exec
closes inherited non-stdio descriptors, clears capabilities, sets no-new-privs
and starts from an explicitly cleared environment with private HOME/TMPDIR.
Namespace init protects its descriptors from same-UID tracing. The FUSE server,
CAS, journal and engine credentials stay outside the guest root and PID view.

## Broker directory

The only engine communication projection is a dedicated per-attempt directory
bound read-only at `/run/rudder/agent`. It is owner-private (`0700`) and contains
mandatory `identity.json` (regular, one link, `0600`) plus optional `broker.sock`
(`0600`, socket when present). Extra entries and symlinks are rejected. The
socket may be absent during lazy broker startup. Binding the directory rather
than individual inodes preserves atomic identity rotation and socket restart.

The parent engine starts the scoped broker outside the sandbox. Guest MCP uses
the projected socket and its own identity. Direct journal fallback is prohibited;
mounting the database to make an old MCP implementation work is not supported.
The parent must keep this directory dedicated throughout its lifetime.

## Checkpoint and recovery proof

A staging lease and retained base roots cover execution through checkpoint.
PID 1 termination kills all remaining namespace threads, including detached
children and nested PID namespaces. The outer wrapper waits for the init to
exit after the kernel's namespace teardown. Only normal init completion plus a
private, nonce-bound report authorizes the supervisor to drain FUSE and create
a consistent checkpoint. Process-group polling and idle timestamps are not
proof. Unexpected wrapper/init death, abort, timeout in teardown or uncertain
I/O retains an orphaned/uncertain record and acknowledges no checkpoint.

The supervisor then performs strict FUSE shutdown and stores a retained
`VfsCheckpoint`. The checksummed local execution record follows durable CAS
publication. This is not atomic with the separate project journal and is not a
fleet scheduling lease. Replay returns an existing committed receipt; uncertain
prior executions require reconciliation rather than a blind restart. Private
credential/home state remains separate from source snapshots.

Linux 6.12's [`zap_pid_ns_processes`](https://github.com/torvalds/linux/blob/v6.12/kernel/pid_namespace.c#L160)
disables new PID allocation, signals remaining tasks and waits until they are
gone before init is reaped. [`do_exit`](https://github.com/torvalds/linux/blob/v6.12/kernel/exit.c#L826)
cancels io_uring before releasing files and publishing process exit;
[`io_uring_cancel_generic`](https://github.com/torvalds/linux/blob/v6.12/io_uring/io_uring.c#L2908)
waits for outstanding task requests. This is the kernel contract behind the
barrier, followed by strict FUSE shutdown and `syncfs`; it is not proof of power
loss durability without the store and private backing fsync protocols.

Uninterruptible kernel or device I/O can delay process reaping and unmount beyond
userspace deadlines. A checkpoint remains blocked until those operations actually
finish. Production recovery still needs an outer VM watchdog and reconciliation;
the passing fixture does not establish a universal termination-time bound.

## Qualification and remaining gates

The [public evidence archive](../../examples/execution/evidence/2026-10-30-final/README.md)
records exact source and binary checksums, runtime versions, fixture scripts,
raw results and cleanup. Canary `876e0ea0100398` used Debian bookworm-slim
`sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e`,
Fly kernel `6.12.114-fly`, two shared CPUs and 2 GiB RAM in `iad`. It had no
services, volumes, app secrets or production environment. Device ownership was
changed only inside that disposable VM. Deletion and absence were verified at
`2026-10-30T13:19:39Z`.

The controller used the actual `TerminalPane` and one-use descriptor handoff as
UID/GID 1500. A static BusyBox shell, Node `18.20.4`, and GCC `12.2.0` compiled
and ran a C program. Assertions covered read-only runtime, empty effective
capabilities/no-new-privs, cleared host environment, private selected home and
environment, denied host journal/CAS/peer paths, denied protected init FDs and
no leaked authority FDs. Real MCP initialization through the projected broker
succeeded; a forged own-token copy failed authentication. Fixture secrets were
absent from terminal output and the source checkpoint's CAS closure.

Unicode and ANSI bytes survived the real PTY. Ctrl+C yielded agent exit 130 and
a retained checkpoint. Five pidfd-witnessed processes, including a detached
writer, exited before normal completion. Killing the supervisor also terminated
all five witnessed processes and left an uncertain `running` record with no
checkpoint. The workload submitted a forced asynchronous io_uring write without
waiting for its completion event; the resulting snapshot contained either its
complete bytes or no bytes. This is a bounded fixture alongside the kernel
contract, not exhaustive qualification of every asynchronous I/O subsystem.

Remaining gates are default launch and reviewed worker boot integration,
provider CLI/runtime profiles, engine owner and view lifecycle wiring,
VCS metadata reminting, automatic reconciliation of uncertain executions, and
preview attribution through the trusted execution/view mapping. An operator
must reconcile uncertain effects; a prior `running` record never authorizes a
blind rerun. These checks establish this profile's behavior on the recorded
kernel, not isolation from kernel vulnerabilities or a general seccomp,
network or resource policy.
