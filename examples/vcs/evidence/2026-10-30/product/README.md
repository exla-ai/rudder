# Product bridge checkpoint

[Provenance](provenance.json) records exact source hashes, versions and check
counts. This is a working-tree checkpoint; its HEAD alone does not identify
the uncommitted implementation. The corresponding sources are in
[vcs-bridge](../../../../../vcs-bridge/) and
[the journal](../../../../../engine/src/journal/vcs.rs).

- [Bridge tests](bridge-tests.log): 25 passed; the explicit mount test is ignored
  by the ordinary suite.
- [Journal tests](journal-tests.log): 31 durability and 13 VCS tests passed.
- [Explicit RO fixture](readonly-fork.log): separately passed in a new Linux
  user/mount namespace, with an unchanged parent-namespace witness, EROFS and
  metadata-only fresh attempt identities.
- [Real CLI log](scanner-cli.log) and [command results](scanner-cli.json): six
  cases, 82 commands, 7.589 seconds. These use the explicit development scanner
  profile, distinct from the owning VFS provider.
- [Default feature check](default-check.log): passed. Optional jj support does
  not activate itself in the product.

Absolute fixture, target and repository paths use the named placeholders, and
trailing empty log lines are trimmed. No binaries, credentials or provider profiles are
included. [SHA256SUMS](SHA256SUMS) binds these files.

Run from a checkout with Rust and the exact lockfile:

```sh
cargo test -p rudder-vcs-bridge --features local-development
cargo test -p rudder-engine --test journal_durability --test journal_vcs
cargo check -p rudder-vcs-bridge
```

Place Cargo targets and temporary fixtures on a filesystem with space. The
explicit RO fixture additionally requires a new `unshare --user --map-root-user
--mount --fork` namespace and `RUDDER_VCS_PARENT_MNT_NS` containing the caller's
mount-namespace witness. Its source documents the safety check. Never run that
ignored test in the caller/host mount namespace.

The [architecture note](../../../../../docs/architecture/vcs-bridge.md) defines
the scope: full immutable metadata traversal with leaf reuse, bounded streaming
and conflict handling, host-owned observation receipts and crash-safe private
operation publication. These checks do not prove a writer barrier, macOS mount,
guest adoption transport, live checkout, O(1) launch or distributed cache speed.
