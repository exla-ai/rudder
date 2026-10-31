# Alpha15 source ownership and live steering

The local checks in checks.json cover actual held engine/database custody, read-only SQLite authorization, authenticated source exchanges, a real child handoff and guest-rotation/host-revocation composition, source finalization and live-only fleet messages. The steady endpoint rate is16/s with64 burst credits; all27 fresh finalization checks remain.

Coverage is composed:1,204 baseline Rust tests,82 final affected tests and the separate execution compatibility cases. JavaScript739/0 with9 skips and real-binary TUI59/59 pass. Source bindings distinguish the locally built candidate revision from the final tag, whose only later change is the Fly image pin. CI/Fly build the final archive separately.

These are foundation checks. New managed context/stop wire, connected model sessions, private-jj acceptance, production build-cache speed and fleet/default migration remain open. No model-provider call or accepted-history permission is inferred.
