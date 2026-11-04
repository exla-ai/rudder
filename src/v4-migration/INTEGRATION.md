# Existing-user authority migration integration (n13)

These modules implement local v4 authority/data-format migration. They do not
replace `src/migration.ts`'s existing local-to-Cloud session transfer or
`cloud/src/project-authority/migration.ts`'s administrator-only PostgreSQL schema
installation. They do not migrate the real fleet. Native registration, shared
writer admission and retained App ownership are wired into the product paths
described below. Manifests, dependencies, shared schemas and the jj pin are unchanged.

## Registration and admission

- `mod v4_migration;` is registered in `native/src/main.rs` (module lives at
  `native/src/v4_migration/mod.rs`). It uses existing native/engine dependencies.
- In legacy/default native engine startup, before owner acquisition, scheduler
  creation, or any authority-mode config write, call
  `v4_migration::refuse_implicit_startup(repository)`. Any migration receipt,
  including malformed/interrupted/completed rollback state, requires explicit
  recovery rather than opening a scheduler. Explicit v4 recovery uses the exact
  committed receipt and current journal state under its owner lock.
- TS `assertLegacyWriterAllowed` includes synchronous marker refusal for all
  run-manager/daemon/board callers, including detached spawn. The async
  `assertNoPendingV4Migration(repository)` from `src/migration.ts` provides
  equivalent admission for callers using that helper.
- Neither guard physically fences incompatible binaries. The trusted host must
  establish confirmed scheduler shutdown, command/effect reconciliation,
  disabled launchers, and fencing of uncontrolled external publishing credentials.
  `Quiescence` requires each fact explicitly. Opening a session also refuses known
  live run/backend/controller PIDs, daemon PID, engine owner PID, malformed or
  linked legacy metadata, and a held `.rudder/engine/owner.lock`. It never kills,
  relabels, or takes over live work. Read-only projector daemons may remain.

## Upgrade

Run all work off the App loop. Keep the actual `MigrationSession` alive across
capture/import/commit and return typed messages to the App. Do not serialize its
lock into a permission token or treat its JSON receipt as a newly granted fence.

1. Stop/drain the actual old owner explicitly and establish `Quiescence`.
2. Open `MigrationSession::open(repository, &quiescence)`, which holds the same OS
   lock as `engine_host`. Do not call while engine_host still owns that lock.
3. Call `captureLegacyProject({repository, bundle, projectId, migrationId,
   sessions})` through the TS helper while retaining that lock. Supply every known
   transcript file per run explicitly; a record with a session ID but no supplied
   transcript fails. Capture keeps an ordinary integration source tree, complete
   local `.git`/`.jj` directories, all legacy run/event/output files, undo history,
   old workspace paths/source, and opaque session bytes. Linked integration history
   must first be exported to an ordinary checkout. The bundle must be outside all
   captured roots. It is private (0700 directories, 0600 blobs/manifests) and must
   stay retained through the compatibility/rollback window. No HOME tree or
   credentials are auto-discovered.
4. Open the intended existing local `Journal`. For a legacy project with no
   journal registration, explicitly create its project with the accepted integration
   head under this same lock first; do not grant scheduler control. Supply that
   exact `OwnerFence`. Do not infer an owner from filesystem JSON or change the
   authority mode through config.
   A normally stopped legacy App supplies its exact inactive fence and current
   run epochs. The migration's `reactivating` phase verifies its durable drain
   proof before acquiring custody for history import under the retained lock.
5. Hash the exact `bundle/manifest.json` bytes with raw SHA256 and call
   `session.prepare(&journal, MigrationRequest{...})`. Bind previous authority,
   previous owner, unique migration ID, new owner ID, the full legacy run list,
   existing run epochs (None for unregistered legacy runs), bundle path and hash.
   Reusing an ID with a different request fails. Unknown/stale/omitted run epochs,
   missing source/history/session inventories and corrupt blobs fail before intent.
6. Repeatedly call `advance` until `Committed`. A normally retired legacy App
   starts at `reactivating`: it verifies the exact normal-drain release and
   reclaims journal custody under a request-bound enrollment identity, without
   creating a scheduler. Its durable intent reconciles an interrupted CAS once.
   The original request keeps the supplied inactive owner and complete inventory.
   Other committed phases are `prepared`,
   `exported`, `importing`, `imported`, `transferring`, `committed`. Importing saves
   one run per step. Only the current in-flight run registration and exact pending
   owner transfer can be reconciled after SQLite commit but before receipt save.
   Unrelated changes fail. The existing Journal transaction changes owner and all
   run epochs together, verifies settled effects/runtime custody through
   `transfer_recovered_owner` and retires stale pending targets. No runtime/attempt,
   capability, scheduler, or backend turn is created by import.
7. Keep historical records available through the legacy adapter and store/show the
   compatibility projections from `legacyRecoveryRecord`, which preserve opaque
   fields, session identity, old worktree names/paths, jj/Git integration receipts,
   terminal statuses and delivery facts. Interrupted active work projects as Paused
   with no process/PTY object. Preserve the raw originals. Connect imported run IDs
   to these projections/transcripts; do not treat a registered run without a
   runtime as permission to schedule it.
8. Publish the native owner projection and enable the single fabric scheduling
   authority only through the current host's existing typed ownership path after
   committed receipt/journal reconciliation. `owner.json` writing, App adoption,
   CLI/machine-operation registration and retained old-client admission call sites
   belong to n13/n5, not this module. No automatic resume is implied by upgrade.

## Rollback and daemonless export

Stop/drain the v4 backend/owner and reconcile its effects first. Open a migration
session with fresh quiescence. `pause_for_rollback` uses the saved owner/run epochs.
For normal v4 work that changed them, use `pause_for_rollback_at` with the host's
explicit current owner and full run inventory. Stale expectations fail. The
rollback intent is saved before `Journal::release_drained_owner`, which verifies
normal runtime/effect drain and invalidates the
owner and all run epochs transactionally. Restart reconciles an interrupted
release rather than repeating it. `rollback_paused` means no active journal owner.

After pause, seal/export v4 source deltas and updated sessions into a separate
recovery bundle using the existing workspace materialization/checkpoint path. Pass
the paused receipt's opaque `rollback_checkpoint` as `rollbackCheckpoint` to
`captureLegacyProject` (or the final argument of `createRecoveryBundle`). This
label binds the new archive to that exact paused checkpoint; it grants no live
authority. Then call `finish_rollback`. It refuses archives without that binding,
verifies the independent retained export and marks
`rolled_back`. Original source/run/history/session state and the new delta export
remain present. No source is overwritten, no old scheduler starts, and no authority
mode config is silently changed. The host's compatible recovery adapter must
explicitly import the selected deltas and acquire a fresh owner before any user
resume. Leave the migration refusal marker in place until that deliberate handoff
is implemented; do not delete it to make an old dashboard start.

`verifyRecoveryBundle`, `exportRecoveryBundle`, and `exportLegacyProjectRecovery`
need ordinary Node filesystem access only. They never open the engine, CAS, a mount
or a daemon socket. They verify typed raw SHA256/size references and materialize
into a new directory, refusing to overwrite an existing checkout. The project
export helper reassembles `.git`/`.jj` beside integration source and emits paused
compatibility records alongside untouched raw run records. It returns exported
root locations, including workspace trees and transcripts. Historical absolute
paths in raw records stay intact; a compatible adapter must select/remap the
returned exported locations explicitly. Restored workspace source is recoverable
without promising that linked old workspace metadata is immediately resumable.

## Gates and limits

- `tests/v4-migration.test.mjs` restores a real jj 0.40.0 fixture after removing
  original source, history, runs, workspace and session files. It also checks
  paused/terminal record compatibility, missing transcripts/workspaces, corruption,
  interrupted capture, path traversal, startup refusal, and existing user data.
- `engine/tests/v4_migration.rs` compiles the actual native module by path against
  real SQLite and OS locks. It exercises every committed upgrade phase, partial
  import, post-commit/pre-receipt uncertainty, rollback, stale/concurrent actors,
  changed runtime epochs, known live legacy owners and corrupt/linked metadata.
- Integrated native/CLI/App lifecycle gates cover the product wiring below.
  Real Mac hardware, Fly, staging, fleet rollout, packaging and release remain
  separate coordinator gates. This local hardening pass uses no Fly services.


## Integrated explicit product entry points

`rudder engine migration --repo <checkout>` reads a bounded version-1 request
from stdin. Its `upgrade` action retains the actual native checkout lock while
calling `__migrate-v4` to capture source, history, records, workspaces and explicitly
selected transcript files, then advances the existing journal migration receipt.
`advance` and `status` recover or inspect the same receipt.
Legacy TS users without a Fabric directory get private local journal storage;
the explicit upgrade request still supplies the accepted head and previous
owner fence before project enrollment. Storage creation grants no authority.

After `committed`, `rudder engine adopt-migration --repo <checkout>` accepts an
`AdoptionRequest` on stdin. Supply the migration ID, exact current journal
`expected_owner`, full `run_epochs` map, format version 1 and all four explicit
quiescence assertions. The real engine constructor retains its checkout lock,
checks the original receipt-pinned archive, exact current owner and complete
inventory before recovering/transferring ownership. It then returns compatibility
records to the App loop. Current records take precedence over retained originals;
both preserve backend session identity. Interrupted work is Paused without a
process or PTY, and recovered plans require fresh approval. Legacy startup
continues to refuse every migration marker. Normal Fabric restart requires the
journal-bound adoption receipt described below; explicit re-adoption requires
the fresh inactive owner and complete run epochs.

For rollback, use `rollback_pause` with the exact current `expected_owner` and
complete `run_epochs`. Then `rollback_export` takes `recovery_bundle`, `sessions`
and a new `destination`. While retaining the same actual checkout lock it seals
post-pause deltas with the paused receipt's opaque checkpoint, marks `rolled_back`
and exports a separate recovery checkout. `recovery_export` takes a new
`destination` and boolean `rollback` to select the original archive or sealed
rollback archive later. Export verifies the exact receipt-pinned manifest hash
and all raw object hashes. It never overwrites the original checkout or archive.

The TS helper's explicit export operation verifies project, migration and optional
rollback checkpoint, reassembles ordinary integration history in the new checkout,
and writes only paused compatibility records under its `.rudder/runs`. Workspace
and transcript paths are mapped to the selected verified exported roots. Raw
original records remain beside that checkout. No scheduler or session is started.
A linked historical workspace's source is retained; provider-specific backend
resume and Mac lifecycle behavior still need their separate platform gates.

Successful adoption now records `v4-migration-adoption-v1` in the actual local
journal before native owner publication. Normal Fabric startup may recognize
that receipt under its retained checkout lock, bound to the committed migration,
canonical checkout, original archive hash and exact current Fabric owner. A
normal released owner permits only its immediately prior adopted epoch and same
owner identity. Each new owner records its own adoption receipt before starting
App. A crash between transfer and that receipt requires explicit adoption again.
No file flag grants this admission, and legacy startup still refuses the original
migration marker. Runtime restart reconciliation and uncertain-custody refusal
remain mandatory. Recovered records remain paused and plans unapproved.

## Hardened recovery checkout handoff

The exported checkout now retains `.rudder/engine/v4-recovery.json`. This is
refusal evidence and archive provenance, never an ownership grant. It remains
incomplete until every paused record and explicit transcript/workspace mapping
has been written. All compatible TS writers and native legacy startup refuse
this marker, including malformed and linked markers. Do not delete it.

After explicit `rollback_export` or `recovery_export`, deliberately start the
returned checkout with `rudder engine start --repo <integrationPath>`. Under
its real checkout lock the native Fabric constructor verifies the completed
export, receipt-pinned archive hash, selected rollback checkpoint and retained
run inventory, acquires a fresh journal owner for that new checkout, imports
history without runtimes and publishes its own owner projection. App adoption
keeps interrupted records Paused, preserves explicit exported transcript links
and requires fresh plan approval. Normal restart repeats these checks and uses
the current journal inventory. The original checkout and its migration refusal
marker remain intact through this separate handoff.

`rollback_pause` accepts `expected_owner` (`current_owner` remains an input
alias) and the complete current `run_epochs`. After normal App shutdown, use
the exact inactive owner returned by the journal. The migration adapter verifies
the immediately preceding durable normal-drain release and reuses it without
starting a scheduler or incrementing epochs again. An inactive flag from an
unqualified release is insufficient. Interrupted release recovery makes the
same drain-proof check, and repeated pause requests retain the checkpoint.

The shared synchronous TS admission guard covers every run-manager, scheduler,
planning writer and scheduler-daemon entry point. Native legacy startup checks
again after acquiring its actual checkout lock. Integrated tests in
`native/src/v4_migration_integration_tests.rs` use the real owner/App lifecycle,
retained source leases, normal shutdown, restart, rollback and separate recovery
ownership; unresolved runtime custody refuses transfer before owner publication.
Fabric selection alone also refuses a legacy journal owner or retained legacy
state without a committed migration or completed separate recovery export.
An `owner.json` authority label cannot override the actual journal's owner.
