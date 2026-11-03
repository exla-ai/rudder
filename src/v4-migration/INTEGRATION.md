# Existing-user authority migration integration (n13)

These modules implement local v4 authority/data-format migration. They do not
replace `src/migration.ts`'s existing local-to-Cloud session transfer or
`cloud/src/project-authority/migration.ts`'s administrator-only PostgreSQL schema
installation. They do not migrate the real fleet. No manifests, dependencies,
shared schemas, jj pin, generated context, or shared registrations were changed.

## Registration and admission

- Register `mod v4_migration;` in `native/src/main.rs` (module lives at
  `native/src/v4_migration/mod.rs`). It uses existing native/engine dependencies.
- In legacy/default native engine startup, before owner acquisition, scheduler
  creation, or any authority-mode config write, call
  `v4_migration::refuse_implicit_startup(repository)`. Any migration receipt,
  including malformed/interrupted/completed rollback state, requires explicit
  recovery rather than opening a scheduler. Explicit v4 recovery uses the exact
  committed receipt and current journal state under its owner lock.
- At TS writer admission, alongside `assertLegacyWriterAllowed`, await
  `assertNoPendingV4Migration(repository)` from `src/migration.ts`. The existing
  synchronous authority guard is outside n6 ownership; add this at its async
  run-manager/daemon/board callers or provide equivalent synchronous admission.
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
5. Hash the exact `bundle/manifest.json` bytes with raw SHA256 and call
   `session.prepare(&journal, MigrationRequest{...})`. Bind previous authority,
   previous owner, unique migration ID, new owner ID, the full legacy run list,
   existing run epochs (None for unregistered legacy runs), bundle path and hash.
   Reusing an ID with a different request fails. Unknown/stale/omitted run epochs,
   missing source/history/session inventories and corrupt blobs fail before intent.
6. Repeatedly call `advance` until `Committed`. Committed phases are `prepared`,
   `exported`, `importing`, `imported`, `transferring`, `committed`. Importing saves
   one run per step. Only the current in-flight run registration and exact pending
   owner transfer can be reconciled after SQLite commit but before receipt save.
   Unrelated changes fail. The existing Journal transaction changes owner and all
   run epochs together, retiring stale pending targets. No runtime/attempt,
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
rollback intent is saved before `Journal::release_owner`, which invalidates the
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
- n13 must run the integrated native/CLI/App lifecycle gates after registering
  these modules/call sites. This worker does not claim that those unowned call sites
  or real Mac hardware, Fly, staging, fleet rollout, packaging or release gates pass.
