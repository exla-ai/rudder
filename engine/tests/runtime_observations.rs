use rudder_engine::journal::*;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn observation() -> RuntimeObservation {
    RuntimeObservation {
        run_id: "target".into(),
        run_epoch: 1,
        attempt_id: "attempt-one".into(),
        backend: "codex".into(),
        execution_profile: "codex-app-server".into(),
        thread_id: Some("thread-one".into()),
        active_turn_id: Some("turn-one".into()),
        last_interrupted_turn_id: None,
        phase: RuntimePhase::Working,
        capabilities: BTreeSet::from(["inspect".into(), "interrupt".into(), "steer".into()]),
        pty_owned: true,
    }
}
fn setup() -> (tempfile::TempDir, Journal, OwnerFence, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut j = Journal::open(dir.path().join("journal.sqlite")).unwrap();
    let owner = j.create_project("p", "engine", "base", 1000).unwrap();
    j.register_run(&owner, "source", 1000).unwrap();
    j.observe_run_attempt(&owner, "target", "attempt-one", 1000)
        .unwrap();
    j.observe_runtime(&owner, &observation(), 1000).unwrap();
    let token = j
        .issue_capability(
            &owner,
            &CapabilityGrant {
                principal: "source-principal".into(),
                run_id: "source".into(),
                run_epoch: 1,
                operations: BTreeSet::from([
                    "agents:find".into(),
                    "command:interrupt".into(),
                    "command:resume".into(),
                ]),
                recipients: BTreeMap::from([("target".into(), 1)]),
                scopes: BTreeSet::from(["project".into()]),
                topics: BTreeSet::new(),
                expires_at_ms: None,
            },
            1000,
        )
        .unwrap();
    (dir, j, owner, token)
}
fn command() -> CommandRequest {
    CommandRequest {
        idempotency_key: "interrupt-key".into(),
        target_run: "target".into(),
        target_epoch: 1,
        kind: "interrupt".into(),
        scope: "project".into(),
        payload: json!({"expected_thread_id":"thread-one","expected_turn_id":"turn-one"}),
    }
}

#[test]
fn observation_is_bound_to_actor_attempt_and_discovery_marks_age() {
    let (_dir, mut j, owner, token) = setup();
    let found = j.find_agent_runtime(&token, 1001).unwrap();
    assert!(
        !found
            .iter()
            .find(|agent| agent.identity.run_id == "target")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stale
    );
    let found = j
        .find_agent_runtime(&token, 1000 + RUNTIME_MAX_AGE_MS + 1)
        .unwrap();
    assert!(
        found
            .iter()
            .find(|agent| agent.identity.run_id == "target")
            .unwrap()
            .runtime
            .as_ref()
            .unwrap()
            .stale
    );
    assert!(j
        .accept_command(&token, &command(), 1000 + RUNTIME_MAX_AGE_MS + 1)
        .is_err());
    let mut forged = observation();
    forged.attempt_id = "other-attempt".into();
    assert!(j.observe_runtime(&owner, &forged, 1002).is_err());
    j.observe_run_attempt(&owner, "target", "new-attempt", 1003)
        .unwrap();
    assert!(j
        .runtime_snapshot(&owner, "target", 1003)
        .unwrap()
        .is_none());
    assert!(j.observe_runtime(&owner, &observation(), 1003).is_err());
}

#[test]
fn explicit_thread_turn_and_current_owned_phase_are_required_at_acceptance() {
    let (_dir, mut j, owner, token) = setup();
    for payload in [
        json!({}),
        json!({"expected_thread_id":"thread-one"}),
        json!({"expected_thread_id":"wrong","expected_turn_id":"turn-one"}),
        json!({"expected_thread_id":"thread-one","expected_turn_id":"old-turn"}),
    ] {
        let mut request = command();
        request.payload = payload;
        assert!(j.accept_command(&token, &request, 1001).is_err());
    }
    let mut closed = observation();
    closed.phase = RuntimePhase::Closed;
    closed.pty_owned = false;
    closed.capabilities = BTreeSet::from(["inspect".into()]);
    j.observe_runtime(&owner, &closed, 1001).unwrap();
    assert!(j.accept_command(&token, &command(), 1002).is_err());
    closed.capabilities.insert("interrupt".into());
    assert!(j.observe_runtime(&owner, &closed, 1002).is_err());
}

#[test]
fn stopped_turn_resume_requires_interrupted_proof_and_pin_is_rechecked_before_dispatch() {
    let (_dir, mut j, owner, token) = setup();
    let accepted = j.accept_command(&token, &command(), 1001).unwrap();
    let mut interrupted = observation();
    interrupted.phase = RuntimePhase::Interrupted;
    interrupted.active_turn_id = None;
    interrupted.last_interrupted_turn_id = Some("turn-one".into());
    interrupted.capabilities = BTreeSet::from(["inspect".into(), "resume".into()]);
    j.observe_runtime(&owner, &interrupted, 1002).unwrap();
    assert!(
        j.claim_effect(&owner, &accepted.command_id, 1003)
            .unwrap()
            .is_none(),
        "accepted old live turn must never bind to a newer phase"
    );
    let mut resume = command();
    resume.kind = "resume".into();
    resume.idempotency_key = "resume-key".into();
    let receipt = j.accept_command(&token, &resume, 1004).unwrap();
    let claim = j
        .claim_effect(&owner, &receipt.command_id, 1005)
        .unwrap()
        .unwrap();
    j.mark_effect_uncertain(&claim, &json!({"transport":"lost"}), 1006)
        .unwrap();
    assert!(j.ready_commands(&owner, 8).unwrap().is_empty());
    j.observe_run_attempt(&owner, "target", "replacement", 1007)
        .unwrap();
    j.reconcile_effect(
        &owner,
        &receipt.command_id,
        &Reconciliation::Completed {
            result: json!({"status":"transport_accepted","turn_id":"old-attempt-new-turn"}),
            evidence: json!({"attempt_id":"attempt-one","backend_receipt":"exact-late-result"}),
        },
        1008,
    )
    .unwrap();
    assert!(
        j.ready_commands(&owner, 8).unwrap().is_empty(),
        "historical completion is not replay under replacement attempt"
    );
}

#[test]
fn batched_heartbeat_is_atomic_when_one_attempt_fence_is_stale() {
    let (_dir, mut j, owner, _token) = setup();
    j.observe_run_attempt(&owner, "second", "second-attempt", 1000)
        .unwrap();
    let mut first = observation();
    first.phase = RuntimePhase::Waiting;
    first.active_turn_id = None;
    first.capabilities = BTreeSet::from(["inspect".into()]);
    let mut invalid = first.clone();
    invalid.run_id = "second".into();
    invalid.attempt_id = "wrong-attempt".into();
    assert!(j
        .observe_runtimes(&owner, &[first.clone(), invalid.clone()], 1002)
        .is_err());
    assert_eq!(
        j.runtime_snapshot(&owner, "target", 1002)
            .unwrap()
            .unwrap()
            .observation
            .phase,
        RuntimePhase::Working
    );
    invalid.attempt_id = "second-attempt".into();
    let snapshots = j.observe_runtimes(&owner, &[first, invalid], 1003).unwrap();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].observed_at_ms, snapshots[1].observed_at_ms);
}
