use super::*;
use serde_json::json;
use std::{collections::BTreeSet, fs, io::Write, path::PathBuf, process::Command};

use crate::journal as recovery_journal;
#[path = "../../../native/src/runtime_recovery/mod.rs"]
mod native_recovery;

#[test]
fn native_restart_projection_preserves_paused_agents_and_quarantines_unjoined_custody() {
    use native_recovery::*;
    let mut f = fixture();
    let expected = ExpectedRuntimeAttempt {
        run_id: "target".into(),
        run_epoch: 1,
        attempt_id: "attempt".into(),
    };
    let uncertain =
        recover_before_replacement(&mut f.journal, &f.owner, &[expected.clone()], 2).unwrap();
    assert!(uncertain.effects.pending.is_empty() && uncertain.effects.uncertain.is_empty());
    assert_eq!(
        uncertain.runtimes,
        vec![RecoveredRuntimeDisposition::Quarantined {
            attempt: expected.clone(),
            reason: RuntimeQuarantineReason::UncertainCustody
        }]
    );
    assert!(
        f.journal.release_drained_owner(&f.owner, 2).is_err(),
        "a lost PTY flag cannot qualify uncertain custody as a normal drain"
    );
    // A completed backend turn can still have an owned interactive process.
    // Crash recovery must quarantine that process rather than treat it as joined.
    f.observation.phase = RuntimePhase::Completed;
    f.observation.active_turn_id = None;
    f.journal
        .observe_runtime(&f.owner, &f.observation, 3)
        .unwrap();
    let completed_but_unjoined =
        recover_before_replacement(&mut f.journal, &f.owner, &[expected.clone()], 3).unwrap();
    assert_eq!(completed_but_unjoined.runtimes, uncertain.runtimes);
    assert!(f.journal.release_drained_owner(&f.owner, 3).is_err());
    f.observation.phase = RuntimePhase::Closed;
    f.observation.active_turn_id = None;
    f.observation.pty_owned = false;
    f.observation.capabilities = ["inspect".into()].into();
    f.journal
        .observe_runtime(&f.owner, &f.observation, 3)
        .unwrap();
    for _ in 0..3 {
        let paused =
            recover_before_replacement(&mut f.journal, &f.owner, &[expected.clone()], 4).unwrap();
        assert_eq!(
            paused.runtimes,
            vec![RecoveredRuntimeDisposition::Paused(expected.clone())]
        );
        assert!(f.journal.ready_commands(&f.owner, 10).unwrap().is_empty());
    }
    let wrong = ExpectedRuntimeAttempt {
        attempt_id: "replacement".into(),
        ..expected
    };
    let stale = recover_before_replacement(&mut f.journal, &f.owner, &[wrong.clone()], 5).unwrap();
    assert_eq!(
        stale.runtimes,
        vec![RecoveredRuntimeDisposition::Quarantined {
            attempt: wrong,
            reason: RuntimeQuarantineReason::ReplacedAttempt
        }]
    );
}

pub(super) struct Fixture {
    pub(super) _dir: tempfile::TempDir,
    pub(super) path: PathBuf,
    pub(super) journal: Journal,
    pub(super) owner: OwnerFence,
    pub(super) token: String,
    pub(super) observation: RuntimeObservation,
}

pub(super) fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let owner = journal.create_project("p", "original", "base", 1).unwrap();
    journal.register_run(&owner, "sender", 1).unwrap();
    journal
        .observe_run_attempt(&owner, "target", "attempt", 1)
        .unwrap();
    journal
        .observe_run_attempt(&owner, "other", "other-attempt", 1)
        .unwrap();
    let observation = RuntimeObservation {
        run_id: "target".into(),
        run_epoch: 1,
        attempt_id: "attempt".into(),
        backend: "codex".into(),
        execution_profile: "codex-app-server".into(),
        thread_id: Some("thread".into()),
        active_turn_id: Some("turn".into()),
        last_interrupted_turn_id: None,
        phase: RuntimePhase::Working,
        capabilities: ["inspect", "steer", "interrupt"].map(String::from).into(),
        pty_owned: true,
    };
    journal.observe_runtime(&owner, &observation, 1).unwrap();
    let token = journal
        .issue_capability(
            &owner,
            &CapabilityGrant {
                principal: "sender".into(),
                run_id: "sender".into(),
                run_epoch: 1,
                operations: ["command:inspect".into()].into(),
                recipients: [("target".into(), 1), ("other".into(), 1)].into(),
                scopes: ["project".into()].into(),
                topics: BTreeSet::new(),
                expires_at_ms: None,
            },
            1,
        )
        .unwrap();
    Fixture {
        _dir: dir,
        path,
        journal,
        owner,
        token,
        observation,
    }
}

pub(super) fn request(key: &str, target: &str) -> CommandRequest {
    CommandRequest {
        idempotency_key: key.into(),
        target_run: target.into(),
        target_epoch: 1,
        kind: "inspect".into(),
        scope: "project".into(),
        payload: json!({"text":key}),
    }
}

// This test is also the deliberately abrupt subprocess entry point. A normal
// suite invocation returns immediately; the parent always launches it exactly
// and asserts its special exit code. No ignored fixture counts as evidence.
#[test]
fn crash_window_child() {
    let Some(path) = std::env::var_os("RUDDER_RECOVERY_CRASH_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let window = std::env::var("RUDDER_RECOVERY_CRASH_WINDOW").unwrap();
    let token = std::env::var("RUDDER_RECOVERY_CRASH_TOKEN").unwrap();
    let mut journal = Journal::open(&path).unwrap();
    let owner = journal.project_state("p").unwrap().owner;
    let accepted = journal
        .accept_command(&token, &request("crash", "target"), 2)
        .unwrap();
    if window != "accepted" {
        let claim = journal
            .claim_effect(&owner, &accepted.command_id, 2)
            .unwrap()
            .unwrap();
        fs::write(
            path.with_extension("claim.json"),
            serde_json::to_vec(&claim).unwrap(),
        )
        .unwrap();
        let mut external = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.with_extension("external"))
            .unwrap();
        writeln!(external, "{}", accepted.command_id).unwrap();
        external.sync_all().unwrap();
        if window == "completed" || window == "acknowledged" {
            journal
                .finish_effect(&claim, &json!({"done":true}), 3)
                .unwrap();
            if window == "acknowledged" {
                let delivery = journal.completion_outbox(&owner, "surface", 10).unwrap();
                journal
                    .acknowledge_completions(&owner, "surface", delivery[0].sequence, 4)
                    .unwrap();
            }
        }
    }
    std::process::exit(73);
}

#[test]
fn actual_engine_crashes_preserve_acceptance_uncertainty_completion_and_acknowledgement() {
    for window in ["accepted", "started", "completed", "acknowledged"] {
        let mut f = fixture();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "journal::runtime::restart_recovery::tests::crash_window_child",
                "--nocapture",
            ])
            .env("RUDDER_RECOVERY_CRASH_PATH", &f.path)
            .env("RUDDER_RECOVERY_CRASH_WINDOW", window)
            .env("RUDDER_RECOVERY_CRASH_TOKEN", &f.token)
            .status()
            .unwrap();
        assert_eq!(child.code(), Some(73), "crash child did not reach {window}");
        f.journal = Journal::open(&f.path).unwrap();
        let recovery = f.journal.recover_runtime_restart(&f.owner, 5).unwrap();
        let original = f
            .journal
            .accept_command(&f.token, &request("crash", "target"), 5)
            .unwrap();
        assert_eq!(
            f.journal
                .events("p", 0, 1000)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "command-accepted")
                .count(),
            1
        );
        let runtime = f
            .journal
            .runtime_snapshot(&f.owner, "target", 5)
            .unwrap()
            .unwrap();
        assert!(!runtime.observation.pty_owned);
        assert_eq!(runtime.observation.phase, RuntimePhase::Uncertain);
        assert_eq!(
            runtime.observation.capabilities,
            ["inspect".to_string()].into()
        );
        match window {
            "accepted" => {
                assert_eq!(recovery.pending.len(), 1);
                assert!(recovery.uncertain.is_empty());
                let claim = f
                    .journal
                    .claim_effect(&f.owner, &original.command_id, 6)
                    .unwrap()
                    .unwrap();
                assert!(f
                    .journal
                    .claim_effect(&f.owner, &original.command_id, 6)
                    .unwrap()
                    .is_none());
                f.journal
                    .finish_effect(&claim, &json!({"done":true}), 6)
                    .unwrap();
            }
            "started" => {
                assert_eq!(recovery.uncertain.len(), 1);
                assert!(f
                    .journal
                    .claim_effect(&f.owner, &original.command_id, 6)
                    .unwrap()
                    .is_none());
                let late: EffectClaim =
                    serde_json::from_slice(&fs::read(f.path.with_extension("claim.json")).unwrap())
                        .unwrap();
                assert!(f
                    .journal
                    .finish_effect(&late, &json!({"done":true}), 6)
                    .is_err());
                let external = fs::read_to_string(f.path.with_extension("external")).unwrap();
                assert_eq!(
                    external.lines().collect::<Vec<_>>(),
                    vec![original.command_id.as_str()]
                );
                f.journal
                    .reconcile_effect(
                        &f.owner,
                        &original.command_id,
                        &Reconciliation::Completed {
                            result: json!({"done":true}),
                            evidence: json!({"observed_external_command_id":external.trim()}),
                        },
                        6,
                    )
                    .unwrap();
            }
            _ => {
                assert!(recovery.pending.is_empty() && recovery.uncertain.is_empty());
                assert_eq!(original.result, Some(json!({"done":true})));
                assert!(f
                    .journal
                    .claim_effect(&f.owner, &original.command_id, 6)
                    .unwrap()
                    .is_none());
                assert_eq!(
                    fs::read_to_string(f.path.with_extension("external"))
                        .unwrap()
                        .lines()
                        .count(),
                    1
                );
            }
        }
        let deliveries = f
            .journal
            .completion_outbox(&f.owner, "surface", 10)
            .unwrap();
        assert_eq!(deliveries.len(), usize::from(window != "acknowledged"));
        assert_eq!(
            deliveries,
            f.journal
                .completion_outbox(&f.owner, "surface", 10)
                .unwrap(),
            "surface read/disconnect must not acknowledge"
        );
        if let Some(last) = deliveries.last() {
            f.journal
                .acknowledge_completions(&f.owner, "surface", last.sequence, 7)
                .unwrap();
        }
        let sequence = f.journal.project_state("p").unwrap().sequence;
        for _ in 0..3 {
            f.journal.recover_runtime_restart(&f.owner, 8).unwrap();
        }
        assert_eq!(
            f.journal.project_state("p").unwrap().sequence,
            sequence,
            "recovery must be idempotent"
        );
        f.journal = Journal::open(&f.path).unwrap();
        assert!(f
            .journal
            .completion_outbox(&f.owner, "surface", 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            f.journal
                .completion_outbox(&f.owner, "another-surface", 10)
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn uncertain_effect_blocks_replacement_until_reconciled_and_stale_epochs_are_rejected() {
    let mut f = fixture();
    let accepted = f
        .journal
        .accept_command(&f.token, &request("started", "target"), 2)
        .unwrap();
    let old = f
        .journal
        .claim_effect(&f.owner, &accepted.command_id, 2)
        .unwrap()
        .unwrap();
    assert!(f
        .journal
        .transfer_recovered_owner(&f.owner, "replacement", 3)
        .is_err());
    f.journal.recover_runtime_restart(&f.owner, 3).unwrap();
    assert!(f
        .journal
        .transfer_recovered_owner(&f.owner, "replacement", 3)
        .is_err());
    f.journal
        .reconcile_effect(
            &f.owner,
            &accepted.command_id,
            &Reconciliation::RetrySafe {
                evidence: json!({"external_service":"confirmed absent"}),
            },
            4,
        )
        .unwrap();
    let receipt = f
        .journal
        .command_receipt(&f.owner, &accepted.command_id)
        .unwrap();
    assert!(
        receipt.result.is_none(),
        "safe retry must discard stale uncertainty result"
    );
    let new = f
        .journal
        .claim_effect(&f.owner, &accepted.command_id, 4)
        .unwrap()
        .unwrap();
    assert_ne!(old.claim_id, new.claim_id);
    assert!(f.journal.finish_effect(&old, &json!({}), 4).is_err());
    f.journal
        .finish_effect(&new, &json!({"done":true}), 4)
        .unwrap();
    assert!(
        f.journal
            .transfer_recovered_owner(&f.owner, "replacement", 5)
            .is_err(),
        "reconciled command effects do not prove original runtime custody"
    );
    f.observation.phase = RuntimePhase::Closed;
    f.observation.pty_owned = false;
    f.observation.active_turn_id = None;
    f.observation.capabilities = ["inspect".into()].into();
    f.journal
        .observe_runtime(&f.owner, &f.observation, 5)
        .unwrap();
    let replacement = f
        .journal
        .transfer_recovered_owner(&f.owner, "replacement", 5)
        .unwrap();
    assert_eq!(replacement.epoch, f.owner.epoch + 1);
    assert!(f
        .journal
        .finish_effect(&new, &json!({"done":true}), 5)
        .is_err());
    assert!(f
        .journal
        .accept_command(&f.token, &request("stale", "target"), 5)
        .is_err());
    assert!(f
        .journal
        .observe_runtime(&f.owner, &f.observation, 5)
        .is_err());
    assert!(f
        .journal
        .observe_runtime(&replacement, &f.observation, 5)
        .is_err());
    assert!(f
        .journal
        .acknowledge_completions(&f.owner, "surface", 1, 5)
        .is_err());
}

#[test]
fn outbox_uses_completion_order_and_durable_monotonic_prefix_acknowledgements() {
    let mut f = fixture();
    let first = f
        .journal
        .accept_command(&f.token, &request("first", "target"), 2)
        .unwrap();
    let first_claim = f
        .journal
        .claim_effect(&f.owner, &first.command_id, 2)
        .unwrap()
        .unwrap();
    let second = f
        .journal
        .accept_command(&f.token, &request("second", "other"), 2)
        .unwrap();
    let second_claim = f
        .journal
        .claim_effect(&f.owner, &second.command_id, 2)
        .unwrap()
        .unwrap();
    f.journal
        .finish_effect(&second_claim, &json!({"second":true}), 3)
        .unwrap();
    let delivered = f
        .journal
        .completion_outbox(&f.owner, "surface", 1)
        .unwrap()
        .remove(0);
    f.journal
        .acknowledge_completions(&f.owner, "surface", delivered.sequence, 4)
        .unwrap();
    f.journal
        .finish_effect(&first_claim, &json!({"first":true}), 5)
        .unwrap();
    let late = f
        .journal
        .completion_outbox(&f.owner, "surface", 1)
        .unwrap()
        .remove(0);
    assert_eq!(late.receipt.command_id, first.command_id);
    assert!(late.sequence > delivered.sequence);
    assert!(f
        .journal
        .acknowledge_completions(&f.owner, "surface", late.sequence + 100, 6)
        .is_err());
    f.journal
        .acknowledge_completions(&f.owner, "surface", late.sequence, 6)
        .unwrap();
    f.journal
        .acknowledge_completions(&f.owner, "surface", delivered.sequence, 6)
        .unwrap();
    assert!(f
        .journal
        .completion_outbox(&f.owner, "surface", 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        f.journal
            .finish_effect(&first_claim, &json!({"first":true}), 7)
            .unwrap(),
        late.receipt
    );
    assert!(f
        .journal
        .finish_effect(&first_claim, &json!({"first":false}), 7)
        .is_err());
    assert!(f
        .journal
        .completion_outbox(&f.owner, "surface", 1)
        .unwrap()
        .is_empty());
}

#[test]
fn historical_completions_backfill_without_execution_and_repeated_recovery_does_not_revive_closed_agents(
) {
    let mut f = fixture();
    let accepted = f
        .journal
        .accept_command(&f.token, &request("historical", "target"), 2)
        .unwrap();
    let claim = f
        .journal
        .claim_effect(&f.owner, &accepted.command_id, 2)
        .unwrap()
        .unwrap();
    f.journal
        .finish_effect(&claim, &json!({"done":true}), 3)
        .unwrap();
    f.journal
        .conn
        .execute("DELETE FROM events WHERE kind='command-result-ready'", [])
        .unwrap();
    f.observation.phase = RuntimePhase::Closed;
    f.observation.pty_owned = false;
    f.observation.active_turn_id = None;
    f.observation.capabilities = ["inspect".into()].into();
    f.journal
        .observe_runtime(&f.owner, &f.observation, 4)
        .unwrap();
    assert_eq!(
        f.journal
            .backfill_completion_outbox(&f.owner, 1, 5)
            .unwrap(),
        1
    );
    assert_eq!(
        f.journal
            .backfill_completion_outbox(&f.owner, 1, 5)
            .unwrap(),
        0
    );
    for _ in 0..3 {
        let recovery = f.journal.recover_runtime_restart(&f.owner, 6).unwrap();
        assert!(recovery.pending.is_empty() && recovery.uncertain.is_empty());
        assert_eq!(
            f.journal
                .runtime_snapshot(&f.owner, "target", 6)
                .unwrap()
                .unwrap()
                .observation
                .phase,
            RuntimePhase::Closed
        );
        assert!(f.journal.ready_commands(&f.owner, 10).unwrap().is_empty());
    }
    assert_eq!(
        f.journal
            .completion_outbox(&f.owner, "surface", 10)
            .unwrap()
            .len(),
        1
    );
}
