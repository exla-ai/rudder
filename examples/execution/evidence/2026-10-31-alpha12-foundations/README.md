# Alpha.12 workspace and network foundations

These records cover held-state workspace construction, strict remote selection
and opt-in network configuration/kernel primitives. They do not establish remote
guest activation, connected egress, accepted source history or full v4 completion.
The source hashes identify the tested main-checkout inputs; paths in the handoff
records identify the original builder logs and fixtures.

- `execution-handoff.json` records preflight/recovery, lock and directory custody,
  owned workspace lifetime, the late-checkpoint correction and focused checks.
- `vfs-handoff.json` records strict selection, reserved namespace/binding formats,
  resource lifetime and compatibility checks, including retained intermediate
  failures and their affected reruns.
- `network-source.json` binds all five network inputs and the local test binary.
  The 21-case suite and six process records are included. Namespace setup,
  parent-witness closure, all-five capability clearing, actual exec readback and
  nested-USER reset refusal succeeded. Two standalone capability modes returned
  Unsupported/EPERM and are not positive qualification of those branches.
- `network-peer-review.md` records the bounded independent source review.

The production build, full suites, tagged CI, npm installs and Fly qualification
are separate release checks. See the
[ownership contract](../../../../docs/architecture/execution-workspace.md) and
[implementation record](../../../../docs/architecture/implementation.md).
