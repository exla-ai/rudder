Exact alpha9 compatibility refusal qualification, 2026-10-30 UTC.

**The missing-owner projection is a real limitation: alpha9's TS guard allows
legacy writes when `.rudder/engine/owner.json` is absent.** A schema5 journal
protects the tested alpha9 native startup paths in the configured Rudder home;
it does not make the TS guard consult that journal. Neither artifact physically
revokes unsupported clients, already-running workers or their publishing rights.
The native version ceiling applies to the entire configured Rudder home because
all its projects share `<home>/fabric/journal.sqlite`. It does not cover an old
client selecting a different home or bypassing cooperative launcher checks.

The [report](report.json) records the actual deployed alpha9 worker image,
native executable hash, flags, timings, PTY output, effective UID and runtime.
The source revision is `11f1f75e5a18555f6c7e257aacc6b4097f425c5e`.
The public npm alpha9 metadata endpoint returned 404 during preparation, so this
qualification used the already-deployed image rather than a registry package.

| Artifact | Verified identity |
| --- | --- |
| Image tag | `registry.fly.io/rudder-workers-exla:4.0.0-alpha.9` |
| Actual image digest | `sha256:939c5593c5ed1232577d575f7ff382448c466a9033874c94a1a684c4ec5d599a` |
| Actual native SHA256 | `c08e44c5feb0fe9b2a4907e725cedd07cab5d2daf887622245e62f40fdfd2c85` |
| Package version | `4.0.0-alpha.9` |
| Owned machine | `8ee325c7151e38` |
| Owned name | `rudder-alpha9-compat-d70afb46` |
| Effective test user | UID 1500 |

The generated projects each contained a manually created `user_version=5`
SQLite journal with one synthetic sentinel table, a managed-looking Reserved
run projection and sidecar, and an otherwise empty Git repository. This is
**not** evidence of an implemented activation transaction, schema migration,
managed-run recovery, provider login, model execution or deployment.

Four owner projection cases were tested: `authority=fabric` with `state=stopped`,
malformed JSON, an unsupported owner format and a missing owner file.

- Twelve native processes ran the exact baked executable: headless
  `engine serve --repo <fixture> --json`, ordinary native startup under a real
  120x24 PTY, then another headless startup to exercise a fresh process after
  the previous owner lock was released. All exited 1 with
  `journal schema is newer than this engine`.
- Across all twelve native starts, owner/run/sidecar/Git file hashes stayed
  unchanged. Creation of the stable `owner.lock` file is excluded from that
  comparison. SQLite logical sentinel data and schema version remained intact;
  `integrity_check` returned `ok` after every case.
- On valid fabric, malformed and unsupported owner projections, the exact
  baked TS `continueRun`, `workerRun` and `scheduleTick` functions refused at
  their existing authority guards: nine refusals with zero intercepted
  network or backend spawn attempts. The board delegates to the tested
  `continueRun` and scheduler entrypoints; no HTTP board server was started.
- On a missing owner file, only the pure guard was invoked and it returned
  `allowed`. No writer was invoked for that negative control. This limitation
  remains visible in the raw result and must not be presented as a refusal.

The caller environment was cleared. Native children had only fresh generated
HOME/RUDDER_HOME paths and inert provider launch traps; the TS probe also
intercepted network and child-process APIs, allowing only read-only Git root
discovery. No personal auth/profile/session store, provider key, active home,
model, scheduler service or volume was supplied. Native launch-trap absence
and rejection before App construction are narrower evidence than a complete
kernel syscall trace.

The [controller](../../qualify-alpha9.mjs) required an empty app secret list and
created a sleep-only VM with empty configured environment, services and mounts.
Its fixture transport used private Fly 6PN, the exact new VM's source address
and a fresh nonce, and verified the payload SHA256 before execution. It never
executed the worker bootstrap or home supervisor. No active home was restarted.

The controller deleted the VM and checked its ID/name/nonce/role were absent.
An [additional inventory](independent-absence.json), executed after the
controller had exited, independently confirmed absence by ID, name and nonce
at `2026-10-30T21:57:18.989Z`. Guest fixture directories were removed before the
VM was destroyed.

The [source manifest](source-sha256.json) and
[source-only archive](frozen-source.tar.gz) include exactly three qualification
sources and fifteen explicitly selected frozen alpha9 product source files.
They include no profiles, journals, sessions or credentials. Archiving read only
the explicit source allowlist; recursive file hashing was confined to newly
generated disposable fixture trees. No private runtime store was traversed.
The [transport payload](payload.tar.gz) contains only the
Python guest and TS probe. [Raw generated output](guest-output.log),
[requested machine config](machine-config.json), [state](state.json) and
[checksums](SHA256SUMS) are retained without trimming or rewriting their data.

The next activation gate must place permanent managed state outside the old
`.rudder/runs/<id>/run.json` discovery namespace, use the current native owner's
actual lock and owned process/pidfd receipts, and explicitly retire legacy
launchers/publishers or fence their credentials. A new flag, sidecar, schema
number or owner projection cannot establish that physical handoff.
