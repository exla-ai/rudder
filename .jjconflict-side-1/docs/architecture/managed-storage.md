# Private managed storage and writer compatibility

Alpha.11 introduces an explicit, inactive managed-project preparation boundary.
It does not migrate a normal project on open or activate a managed scheduler.
The [managed launch contract](managed-launch.md) and its remaining execution,
networking and source-acceptance gates still apply.

## Namespace and preparation

The trusted owner supplies canonical home, repository, project and journal
identities. Durable managed data lives under
`<home>/fabric/managed-projects/<sha256(project_id)>`, outside legacy
`.rudder/runs` discovery. Directories are owned and private; bounded canonical
records reject symlinks, foreign ownership and unsafe modes. A managed row's
trusted store location is an in-memory type, never reconstructed from legacy
JSON, cwd or a child environment.

Preparation is an explicit operation off the App loop:

1. Hold the actual exclusive checkout owner lock and current journal fence;
   establish that known legacy owners and children are quiescent.
2. Inspect any experimental legacy records and bind the exact bounded recovery
   plan. Preserve original files and identities.
3. Fsync the private preparation intent and directory entries.
4. In one owner-fenced SQLite transaction, install schema5, the project's
   minimum writer protocol, an event and an idempotent preparation receipt.
5. Persist the matching private receipt and recovery snapshots before admitting
   later preparation work.

Ordinary schema4 opens stay at schema4. Schema5 is a compatibility boundary for
the whole local journal, while each prepared project has its own writer-floor
receipt. Existing intent alone, a committed floor alone, corrupt evidence or a
missing private receipt all prevent a normal Native legacy startup before owner
transfer or `owner.json` rewriting. Explicit recovery validates the original
request and retained evidence. It does not turn the historical receipt's owner
into a current execution grant. The current legacy App loop refuses prepared
ownership before constructing App.

## Run evidence and restart

A reserved attempt is observed in the journal before preparation. Its private
immutable reservation is fsynced before helper effects; its exact workspace
assignment is fsynced before capability publication. Permanent checkpoints,
private VCS history and recovery data stay in the private namespace. Unix
endpoints may use a separately selected, private short directory; their saved
paths do not prove a live process or authorize reconnection.

Restart reads the explicit store and receipt-scoped journal observations. The
bounded typed observation API covers a crash between journal observation and
creation of a run directory. It returns historical run/attempt/sequence data,
never a session, source, live fence or grant. Malformed observations, incomplete
files and exhausted recovery budgets stay visible as recovery errors or
quarantine. No legacy-run fallback is allowed.

Recovered rows are orphaned, with no live PTY or resumable legacy workspace.
An immutable run identity pins its original attempt; automatic retry/resume is
not implemented by this format. Experimental legacy originals remain intact.
Legacy adoption, watched-screen mirroring, row deletion, context propagation and
history operations cannot reinterpret or remove managed recovery data. Mutable
UI projections use a bounded background queue and are not durability receipts.

## Limits and verification

This is cooperative compatibility. TypeScript clients still rely on the owner
projection and do not enforce this journal floor when that projection is absent.
A different home or a process outside this owner's lock is not physically
revoked by schema5 either.
Production cutover still needs retirement of incompatible writers and children,
source/publication credential fencing and a qualified replacement scheduler.
Do not delete old recovery records or change the home to bypass a refusal.

Checks exercise idempotency, owner mismatch, crash boundaries, unchanged schema4,
receipt-scoped restart paging, malformed/missing data, retained experimental
records and legacy-path refusal. An explicit disposable test drives the exact
tagged alpha.10 Linux CI binary against schema5 and checks that it exits without
rewriting the owner row or owner projection. That artifact is tied to its tagged
CI run; alpha.10 was not published to npm. Public release evidence must preserve
that distinction.
