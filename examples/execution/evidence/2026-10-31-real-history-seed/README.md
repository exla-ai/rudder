# Real Git history on a read-only shared seed

Qualified on Fly.io at **2026-10-31T23:44:30.566Z**. This is the exact Main
working-tree fixture, not a tagged release or production activation.

The root-owned Rust producer serves the retained seed read-only to UID1500.
The child imports an independent CAS with the same exact seed reference and
creates two private metadata forks. Both reopen the accepted commit/change/tree,
operation, integration bookmark, Git HEAD and reachable parent blob bytes.
Only fork A writes a new private blob/tree/commit/operation and Git HEAD.
A fresh fork B remains unchanged and cannot resolve A's new commit object.

Root and UID1500 writes fail EROFS. The original reader remains held through
actual child exit/reap. A real open directory provokes EBUSY and preserves the
mount/pin; after close, normal unmount and exact original-pin cleanup succeed.
The controller destroys only its owned VM and verifies absence twice. Seven
unchanged stock-worker executable checks also pass. The run made 23 provider
calls and no model/auth/profile calls. App secrets were checked empty.

This does not qualify production Root IPC/bootstrap, Engine/Journal acceptance,
Spec3 execution, model execution, distributed performance or macOS. The source
ownership in this fixture is test-owned and is not an EngineSourceLease.

`qualification.json` contains the actual kernel/history receipt and cleanup
checks. `source-inputs.json`, `artifact-seal.json` and `derived-image.json` bind
frozen working source, cached/revalidated compiler output, ELF and immutable
image. The base is alpha.23 revision ca3eb6c; the separately added test ELF comes
from the recorded working tree on 4885ef6. It is not that release's native binary.
No ELF is committed. The Dockerfile adds only that hash-recorded test binary.

The included controller/guest transport is the actual run harness. Its selftest
uses five mocked scenarios and cannot qualify a real run; the real proof is
separate. Raw provider inventories, credentials and signed URLs are omitted.
The paired child must never be run directly. The one root test filter is:

```
root_seed_mount::real_history_tests::root_seed_real_history_kernel_qualification
```
