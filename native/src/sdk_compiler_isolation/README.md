# Compiler boundary component

The existing helper embeds `compiler_sdk_confinement.py` with
`compiler_sdk_selected.py`. No module registration or helper call-site change is
needed. The selected observer passes the union of the job's declared source
files to `SdkCompilerReadPolicy`. Every compiler in that job sees that same
union. Per-target visibility is not claimed.

The immutable SDK root, source and rules come from the existing authenticated
Root/helper custody. The boundary additionally requires all of the following:

- Original non-dumpable, no-new-privileges observer with all capability sets zero.
- Original fresh PID-namespace proc mount held only by observer descriptor.
- Exactly six chroot-visible mounts: SDK root, workspace, source, frontend,
  output and temporary directory. Inputs are read-only. Only SDK/frontend mounts
  permit native execution. Output and temporary mounts are private and noexec.
- Source files exactly match the job declarations. Symlinks, hardlinks, special
  files, unexpected mounts and extra files refuse before compiler startup.
- Empty pipe stdin and separate writable pipe stdout/stderr. The pre-exec hook
  explicitly preserves these three descriptors and closes every other descriptor
  on exec, including aliases of the observer's proc descriptor.
- Native little-endian 64-bit x86_64 or aarch64 Linux syscall ABI. Foreign audit
  architectures and x32 calls terminate the process. Namespace clone flags and
  nonzero upper clone flags refuse, while normal threads/forks remain supported.
  clone3 returns ENOSYS for libc fallback. Mount, namespace, ptrace, process-memory,
  file-handle, pidfd-getfd, memfd, execveat, io_uring, SysV IPC and keyring routes
  refuse. There is no weaker retry when a required mechanism is unavailable.

The v3 observation includes the canonical SHA256 of `policy.json`, source union
validation, actual mount count and the child pre-exec denial probes. The Rust
validator requires these fields and rejects old observations and unknown fields.
The inline Python mechanism policy is checked against this JSON by the kernel
test suite. The original v2 input-policy digest remains independently checked;
the action's reusable-cache permission remains false. Before any future cache
admission, the integrator must include this mechanism policy in the action's keyed
rules and complete E3 effect/undeclared-input qualification.

`noexec` plus memfd/execveat denial prevents native execution or executable file
mappings from writable mounts. It is not a semantic code-execution boundary for
SDK interpreters or anonymous JIT memory. Arbitrary shell hermeticity,
deterministic execution and reusable-cache admission are not established.

## Local verification

Run `python3 -I native/src/compiler_sdk_confinement_tests.py -v`, or the single
Node wrapper through `/workspace/rudder-build/rnpm exec -- node --test
tests/sdk-compiler-isolation.test.mjs`. The Node wrapper skips explicitly on
non-Linux hosts or when original UID/GID are not 1500. The direct Python suite
requires those identities, namespace/mount/seccomp support, Python, GCC static
link support and writable fixture storage in `/workspace`. Unsupported required
kernel mechanisms fail the component tests; they never enable an unconfined run.
The i386 foreign-ABI execution probe is x86_64-only and explicitly skips on ARM.

The static C executable is an adversarial syscall probe, not an SDK or compiler.
The suite exercises real exec descriptor closure, absent proc/FD/path routes,
read-only source, private output writes, native output/FD execution denial,
executable file mmap denial, symlink/hardlink routes, every denied syscall,
namespace/high-word clone flags, normal thread/fork inheritance, x32/i386 refusal,
sealed request consumption and malformed source/mount refusal.

These are local kernel component results. Even when the local kernel identifies
as `6.12.114-fly`, they do not qualify a deployed worker, Root admission, the pinned
SDK compiler process tree or a release. The local SDK harness owner must exercise
the actual supported compilers; the sole Fly qualification owner must establish
the deployed result and original cleanup/custody gates.
