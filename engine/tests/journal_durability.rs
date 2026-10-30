use rudder_engine::journal::*;
use rudder_engine::protocol::{ObjectKind, ObjectRef};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

const NOW: i64 = 1000;

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    journal: Journal,
    owner: OwnerFence,
    a: String,
    b: String,
    c: String,
}

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| s.to_string()).collect()
}
fn grant(run: &str, epoch: u64) -> CapabilityGrant {
    CapabilityGrant {
        principal: format!("principal-{run}"),
        run_id: run.into(),
        run_epoch: epoch,
        operations: set(&[
            "command:steer",
            "message:send",
            "mailbox:read",
            "mailbox:ack",
            "decision:accept",
            "topics:read",
            "agents:find",
        ]),
        recipients: ["a", "b", "c"]
            .into_iter()
            .map(|id| (id.to_string(), epoch))
            .collect(),
        scopes: set(&["project"]),
        topics: set(&["api", "policy:build"]),
        expires_at_ms: None,
    }
}
fn working_runtime(journal: &mut Journal, owner: &OwnerFence, run: &str, now: i64) {
    let attempt_id=format!("fixture-attempt-{run}");
    let run_epoch=journal.observe_run_attempt(owner, run, &attempt_id, now).unwrap();
    journal.observe_runtime(owner, &RuntimeObservation {
        run_id:run.into(), run_epoch, attempt_id, backend:"codex".into(),
        execution_profile:"codex-app-server".into(), thread_id:Some("thread-1".into()),
        active_turn_id:Some("turn-1".into()), last_interrupted_turn_id:None,
        phase:RuntimePhase::Working,capabilities:set(&["inspect","steer","interrupt"]),pty_owned:true,
    }, now).unwrap();
}
fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("journal.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let owner = journal
        .create_project("p", "engine-1", "base", NOW)
        .unwrap();
    for run in ["a", "b", "c"] {
        journal.register_run(&owner, run, NOW).unwrap();
    }
    for run in ["b", "c"] { working_runtime(&mut journal, &owner, run, NOW); }
    let a = journal
        .issue_capability(&owner, &grant("a", 1), NOW)
        .unwrap();
    let b = journal
        .issue_capability(&owner, &grant("b", 1), NOW)
        .unwrap();
    let c = journal
        .issue_capability(&owner, &grant("c", 1), NOW)
        .unwrap();
    Fixture {
        _dir: dir,
        path,
        journal,
        owner,
        a,
        b,
        c,
    }
}
fn command(key: &str) -> CommandRequest {
    CommandRequest {
        idempotency_key: key.into(),
        target_run: "b".into(),
        target_epoch: 1,
        kind: "steer".into(),
        scope: "project".into(),
        payload: json!({"instruction":"inspect the build","expected_thread_id":"thread-1","expected_turn_id":"turn-1"}),
    }
}
fn mail(key: &str) -> MessageRequest {
    MessageRequest {
        idempotency_key: key.into(),
        recipient_run: "b".into(),
        recipient_epoch: 1,
        scope: "project".into(),
        intent: MessageIntent::Inform,
        body: json!({"text":"context"}),
        expires_at_ms: NOW + 10000,
        priority: MessagePriority::Normal,
        conversation_id: None,
        request_id: None,
        reply_to: None,
        supersedes: None,
        topic_versions: Default::default(),
        artifacts: vec![],
    }
}
fn decision(key: &str, version: u64) -> DecisionRequest {
    DecisionRequest {
        idempotency_key: key.into(),
        topic: "api".into(),
        expected_version: version,
        scope: "project".into(),
        statement: json!({"contract":"v2"}),
    }
}
fn integration(key: &str, head: &str) -> IntegrationRequest {
    let source = ObjectRef::from_bytes(ObjectKind::SourceRoot, b"verified source metadata");
    let environment = ObjectRef::from_bytes(
        ObjectKind::EnvironmentRoot,
        b"verified environment metadata",
    );
    IntegrationRequest {
        idempotency_key: key.into(),
        expected_head: "base".into(),
        new_head: head.into(),
        proposed_source_root: source.clone(),
        proposed_environment_root: environment.clone(),
        versions: [("api".into(), 0), ("policy:build".into(), 0)]
            .into_iter()
            .collect(),
        evidence: IntegrationEvidence {
            tested_head: "base".into(),
            tested_source_root: source,
            tested_environment_root: environment,
            action_results: vec![ObjectRef::from_bytes(
                ObjectKind::ActionResult,
                b"passed action",
            )],
            checks: json!({"canonical-tests":"passed"}),
        },
    }
}

#[test]
fn acceptance_outbox_and_event_are_one_transaction() {
    let mut f = fixture();
    let before = f.journal.project_state("p").unwrap().sequence;
    let db = rusqlite::Connection::open(&f.path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_effect BEFORE INSERT ON effects BEGIN SELECT RAISE(ABORT,'simulated storage failure'); END;").unwrap();
    assert!(f
        .journal
        .accept_command(&f.a, &command("one"), NOW)
        .is_err());
    assert_eq!(f.journal.project_state("p").unwrap().sequence, before);
    assert_eq!(
        db.query_row("SELECT count(*) FROM commands", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_effect").unwrap();
    let accepted = f
        .journal
        .accept_command(&f.a, &command("one"), NOW)
        .unwrap();
    assert_eq!(accepted.accepted_sequence, before + 1);
    drop(f.journal);
    let reopened = Journal::open(&f.path).unwrap();
    assert_eq!(
        reopened
            .command_receipt(&f.owner, &accepted.command_id)
            .unwrap(),
        accepted
    );
}

#[test]
fn duplicate_commands_preserve_receipt_result_and_canonical_payload() {
    let mut f = fixture();
    let mut request = command("same");
    request.payload = serde_json::from_str(r#"{"b":1.5,"a":{"z":2,"c":3},"expected_thread_id":"thread-1","expected_turn_id":"turn-1"}"#).unwrap();
    let first = f.journal.accept_command(&f.a, &request, NOW).unwrap();
    request.payload = serde_json::from_str(r#"{"expected_turn_id":"turn-1","a":{"c":3,"z":2},"b":1.5,"expected_thread_id":"thread-1"}"#).unwrap();
    assert_eq!(
        f.journal.accept_command(&f.a, &request, NOW + 1).unwrap(),
        first
    );
    let claim = f
        .journal
        .claim_effect(&f.owner, &first.command_id, NOW)
        .unwrap()
        .unwrap();
    let result = json!({"backend":"accepted","turn":"t1"});
    let done = f.journal.finish_effect(&claim, &result, NOW).unwrap();
    assert_eq!(
        f.journal.accept_command(&f.a, &request, NOW + 2).unwrap(),
        done
    );
    assert_eq!(f.journal.finish_effect(&claim, &result, NOW).unwrap(), done);
    assert!(f
        .journal
        .finish_effect(&claim, &json!({"different":true}), NOW)
        .is_err());
    request.payload = json!({"instruction":"different body"});
    assert!(f
        .journal
        .accept_command(&f.a, &request, NOW)
        .unwrap_err()
        .to_string()
        .contains("idempotency"));
}

#[test]
fn simultaneous_duplicate_accepts_and_claims_have_one_winner() {
    let f = fixture();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = f.path.clone();
            let token = f.a.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut j = Journal::open(path).unwrap();
                barrier.wait();
                j.accept_command(&token, &command("race"), NOW).unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(receipts[0], receipts[1]);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = f.path.clone();
            let owner = f.owner.clone();
            let id = receipts[0].command_id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut j = Journal::open(path).unwrap();
                barrier.wait();
                j.claim_effect(&owner, &id, NOW).unwrap().is_some()
            })
        })
        .collect();
    let winners = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
    let state = f
        .journal
        .command_receipt(&f.owner, &receipts[0].command_id)
        .unwrap();
    assert_eq!(state.state, CommandState::Started);
}

#[test]
fn started_effect_is_uncertain_after_reopen_and_not_blindly_replayed() {
    let mut f = fixture();
    let first = f
        .journal
        .accept_command(&f.a, &command("first"), NOW)
        .unwrap();
    let second = f
        .journal
        .accept_command(&f.a, &command("second"), NOW)
        .unwrap();
    assert!(f
        .journal
        .claim_effect(&f.owner, &second.command_id, NOW)
        .unwrap()
        .is_none());
    let old_claim = f
        .journal
        .claim_effect(&f.owner, &first.command_id, NOW)
        .unwrap()
        .unwrap();
    drop(f.journal);
    let mut j = Journal::open(&f.path).unwrap();
    // Merely opening does not replay or mutate recovery state.
    assert_eq!(
        j.command_receipt(&f.owner, &first.command_id)
            .unwrap()
            .state,
        CommandState::Started
    );
    let recovery = j.recover(&f.owner, NOW).unwrap();
    assert_eq!(recovery.pending, vec![second.clone()]);
    assert_eq!(recovery.uncertain.len(), 1);
    assert!(j
        .claim_effect(&f.owner, &first.command_id, NOW)
        .unwrap()
        .is_none());
    assert!(j
        .claim_effect(&f.owner, &second.command_id, NOW)
        .unwrap()
        .is_none());
    assert!(j.finish_effect(&old_claim, &Value::Null, NOW).is_err());
    assert!(j
        .reconcile_effect(
            &f.owner,
            &first.command_id,
            &Reconciliation::RetrySafe {
                evidence: Value::Null
            },
            NOW
        )
        .is_err());
    j.reconcile_effect(
        &f.owner,
        &first.command_id,
        &Reconciliation::Completed {
            result: json!({"observed":"already happened"}),
            evidence: json!({"external_receipt":"effect-1"}),
        },
        NOW,
    )
    .unwrap();
    assert!(j
        .claim_effect(&f.owner, &second.command_id, NOW)
        .unwrap()
        .is_some());
}

#[test]
fn explicit_safe_retry_receives_a_new_claim_and_rejects_old_completion() {
    let mut f = fixture();
    let accepted = f
        .journal
        .accept_command(&f.a, &command("retry"), NOW)
        .unwrap();
    let old = f
        .journal
        .claim_effect(&f.owner, &accepted.command_id, NOW)
        .unwrap()
        .unwrap();
    f.journal.recover(&f.owner, NOW).unwrap();
    f.journal
        .reconcile_effect(
            &f.owner,
            &accepted.command_id,
            &Reconciliation::RetrySafe {
                evidence: json!({"service":"confirmed no effect"}),
            },
            NOW,
        )
        .unwrap();
    let new = f
        .journal
        .claim_effect(&f.owner, &accepted.command_id, NOW)
        .unwrap()
        .unwrap();
    assert_ne!(new.claim_id, old.claim_id);
    assert!(f.journal.finish_effect(&old, &json!({}), NOW).is_err());
    f.journal
        .finish_effect(&new, &json!({"ok":true}), NOW)
        .unwrap();
}

#[test]
fn owner_transfer_fences_old_claims_tokens_and_target_epochs() {
    let mut f = fixture();
    let started = f
        .journal
        .accept_command(&f.a, &command("started"), NOW)
        .unwrap();
    let pending = f
        .journal
        .accept_command(&f.a, &command("pending"), NOW)
        .unwrap();
    let claim = f
        .journal
        .claim_effect(&f.owner, &started.command_id, NOW)
        .unwrap()
        .unwrap();
    let next = f.journal.transfer_owner(&f.owner, "engine-2", NOW).unwrap();
    assert_eq!(next.epoch, f.owner.epoch + 1);
    assert_eq!(f.journal.run_epoch("p", "a").unwrap(), 2);
    assert!(f
        .journal
        .accept_command(&f.a, &command("stale"), NOW)
        .is_err());
    assert!(f.journal.inbox(&f.b, 10, NOW).is_err());
    assert!(f.journal.finish_effect(&claim, &json!({}), NOW).is_err());
    let recovered = f.journal.recover(&next, NOW).unwrap();
    assert_eq!(recovered.uncertain[0].command_id, started.command_id);
    assert!(recovered.pending.is_empty());
    let retired = f.journal.command_receipt(&next, &pending.command_id).unwrap();
    assert_eq!(retired.state, CommandState::Completed);
    assert_eq!(retired.result.unwrap()["status"], "rejected");
    assert!(f
        .journal
        .claim_effect(&next, &pending.command_id, NOW)
        .is_err());
    assert!(f
        .journal
        .reconcile_effect(
            &next,
            &started.command_id,
            &Reconciliation::RetrySafe {
                evidence: json!({"safe":true})
            },
            NOW
        )
        .is_err());
    let token = f
        .journal
        .issue_capability(&next, &grant("a", 2), NOW)
        .unwrap();
    let mut request = command("new-attempt");
    request.target_epoch = 2;
    working_runtime(&mut f.journal, &next, "b", NOW);
    let fresh = f.journal.accept_command(&token, &request, NOW).unwrap();
    assert!(
        f.journal
            .claim_effect(&next, &fresh.command_id, NOW)
            .unwrap()
            .is_some(),
        "old-epoch pending/uncertain work must not block current attempt"
    );
    f.journal
        .reconcile_effect(
            &next,
            &started.command_id,
            &Reconciliation::Completed {
                result: json!({"retired":true}),
                evidence: json!({"inspected":true}),
            },
            NOW,
        )
        .unwrap();
}

#[test]
fn graceful_release_requires_explicit_new_fence_and_reminted_capabilities() {
    let mut f = fixture();
    f.journal.release_owner(&f.owner, NOW).unwrap();
    let state = f.journal.project_state("p").unwrap();
    assert!(!state.owner_active);
    assert_eq!(state.owner.epoch, 2);
    assert!(f.journal.find_agents(&f.a, NOW).is_err());
    assert!(f.journal.transfer_owner(&f.owner, "stale", NOW).is_err());
    let new = f
        .journal
        .transfer_owner(&state.owner, "reopened", NOW)
        .unwrap();
    assert_eq!(new.epoch, 3);
    assert_eq!(f.journal.run_epoch("p", "a").unwrap(), 3);
    assert!(f.journal.find_agents(&f.a, NOW).is_err());
    let token = f
        .journal
        .issue_capability(&new, &grant("a", 3), NOW)
        .unwrap();
    assert_eq!(f.journal.find_agents(&token, NOW).unwrap().len(), 3);
}

#[test]
fn project_recovery_does_not_mutate_other_projects() {
    let mut f = fixture();
    let second = f
        .journal
        .create_project("other", "engine-other", "base", NOW)
        .unwrap();
    for run in ["a", "b", "c"] {
        f.journal.register_run(&second, run, NOW).unwrap();
    }
    working_runtime(&mut f.journal, &second, "b", NOW);
    let token = f
        .journal
        .issue_capability(&second, &grant("a", 1), NOW)
        .unwrap();
    let accepted = f
        .journal
        .accept_command(&token, &command("other"), NOW)
        .unwrap();
    f.journal
        .claim_effect(&second, &accepted.command_id, NOW)
        .unwrap()
        .unwrap();
    f.journal.recover(&f.owner, NOW).unwrap();
    assert_eq!(
        f.journal
            .command_receipt(&second, &accepted.command_id)
            .unwrap()
            .state,
        CommandState::Started
    );
}

#[test]
fn capabilities_limit_scope_identity_discovery_and_epoch_on_every_agent_operation() {
    let mut f = fixture();
    let mut limited = grant("a", 1);
    limited.recipients = [("b".to_string(), 1)].into_iter().collect();
    limited.topics.clear();
    let token = f.journal.issue_capability(&f.owner, &limited, NOW).unwrap();
    let found = f.journal.find_agents(&token, NOW).unwrap();
    assert_eq!(
        found.iter().map(|a| a.run_id.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    let mut request = mail("identity");
    request.body = json!({"author":"user"});
    let accepted = f.journal.send_message(&token, &request, NOW).unwrap();
    assert_eq!(accepted.author, "principal-a");
    assert!(f
        .journal
        .ack_message(&token, &accepted.message_id, NOW)
        .is_err());
    request.idempotency_key = "scope".into();
    request.scope = "other-project".into();
    assert!(f.journal.send_message(&token, &request, NOW).is_err());
    request.scope = "project".into();
    request.recipient_run = "c".into();
    assert!(f.journal.send_message(&token, &request, NOW).is_err());
    assert!(f
        .journal
        .accept_decision(&token, &decision("deny", 0), NOW)
        .is_err());
    f.journal.advance_run_epoch(&f.owner, "b", 1, NOW).unwrap();
    working_runtime(&mut f.journal, &f.owner, "b", NOW);
    assert!(f
        .journal
        .send_message(&token, &mail("old-target"), NOW)
        .is_err());
    assert!(f
        .journal
        .accept_command(&token, &command("old-target"), NOW)
        .is_err());
    let mut fresh = mail("fresh-target");
    fresh.recipient_epoch = 2;
    assert!(
        f.journal.send_message(&token, &fresh, NOW).is_err(),
        "old grants must not control a replacement attempt"
    );
    assert_eq!(
        f.journal
            .find_agents(&token, NOW)
            .unwrap()
            .iter()
            .map(|a| a.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    let mut rebound = limited.clone();
    rebound.recipients.insert("b".into(), 2);
    let rebound_token = f.journal.issue_capability(&f.owner, &rebound, NOW).unwrap();
    f.journal.send_message(&rebound_token, &fresh, NOW).unwrap();
    f.journal.revoke_capability(&f.owner, &token, NOW).unwrap();
    assert!(f.journal.find_agents(&token, NOW).is_err());
    let mut expiring = grant("a", 1);
    expiring.recipients.insert("b".into(), 2);
    expiring.expires_at_ms = Some(NOW + 1);
    let secret = f
        .journal
        .issue_capability(&f.owner, &expiring, NOW)
        .unwrap();
    assert!(f.journal.find_agents(&secret, NOW + 1).is_err());
    let db = rusqlite::Connection::open(&f.path).unwrap();
    let persisted: String = db
        .query_row(
            "SELECT group_concat(grant_json) FROM capabilities",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!persisted.contains(&secret));
    assert!(f.journal.find_agents("forged-token", NOW).is_err());
}

#[test]
fn ack_holes_do_not_advance_cursor_but_other_recipient_sequence_gaps_do() {
    let mut f = fixture();
    let first = f.journal.send_message(&f.a, &mail("first"), NOW).unwrap();
    let mut other = mail("other");
    other.recipient_run = "c".into();
    let gap = f.journal.send_message(&f.a, &other, NOW).unwrap();
    let mut urgent = mail("second");
    urgent.priority = MessagePriority::Urgent;
    let second = f.journal.send_message(&f.a, &urgent, NOW).unwrap();
    assert!(first.sequence < gap.sequence && gap.sequence < second.sequence);
    assert_eq!(
        f.journal.inbox(&f.b, 10, NOW).unwrap()[0].message_id,
        second.message_id
    );
    assert_eq!(
        f.journal
            .ack_message(&f.b, &second.message_id, NOW)
            .unwrap()
            .sequence,
        0
    );
    assert_eq!(
        f.journal
            .ack_message(&f.b, &first.message_id, NOW)
            .unwrap()
            .sequence,
        second.sequence
    );
    let before = f.journal.project_state("p").unwrap().sequence;
    assert_eq!(
        f.journal
            .ack_message(&f.b, &second.message_id, NOW)
            .unwrap()
            .sequence,
        second.sequence
    );
    assert_eq!(f.journal.project_state("p").unwrap().sequence, before);
    drop(f.journal);
    let mut j = Journal::open(&f.path).unwrap();
    assert_eq!(
        j.mailbox_cursor(&f.b, NOW).unwrap().sequence,
        second.sequence
    );
    assert!(j.inbox(&f.b, 10, NOW).unwrap().is_empty());
    assert_eq!(j.inbox(&f.c, 10, NOW).unwrap().len(), 1);
    let db = rusqlite::Connection::open(&f.path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3,
        "acks must retain durable reply records"
    );
}

#[test]
fn mailbox_send_intent_rolls_back_atomically_when_outbox_fails() {
    let mut f = fixture();
    let db = rusqlite::Connection::open(&f.path).unwrap();
    let before = f.journal.project_state("p").unwrap().sequence;
    db.execute_batch("CREATE TRIGGER fail_delivery BEFORE INSERT ON deliveries BEGIN SELECT RAISE(ABORT,'outbox failed'); END;").unwrap();
    assert!(f.journal.send_message(&f.a, &mail("atomic"), NOW).is_err());
    assert_eq!(f.journal.project_state("p").unwrap().sequence, before);
    assert_eq!(
        db.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn replies_are_durable_correlated_and_addressed_only_to_original_attempt() {
    let mut f = fixture();
    let mut request = mail("question");
    request.intent = MessageIntent::Question;
    request.request_id = Some("request-1".into());
    request.conversation_id = Some("conversation".into());
    let question = f.journal.send_message(&f.a, &request, NOW).unwrap();
    let mut reply = mail("reply");
    reply.recipient_run = "a".into();
    reply.intent = MessageIntent::Answer;
    reply.reply_to = Some(question.message_id.clone());
    reply.conversation_id = request.conversation_id.clone();
    assert!(f.journal.send_message(&f.c, &reply, NOW).is_err());
    let answer = f.journal.send_message(&f.b, &reply, NOW).unwrap();
    assert_eq!(answer.correlation_id, Some("request-1".into()));
    assert_eq!(f.journal.send_message(&f.b, &reply, NOW).unwrap(), answer);
    reply.body = json!({"different":true});
    assert!(f.journal.send_message(&f.b, &reply, NOW).is_err());
    drop(f.journal);
    let mut reopened = Journal::open(&f.path).unwrap();
    assert_eq!(reopened.inbox(&f.a, 10, NOW).unwrap(), vec![answer]);
}

#[test]
fn expiry_and_explicit_skip_resolve_holes_without_erasing_history() {
    let mut f = fixture();
    let mut expiring = mail("expires");
    expiring.expires_at_ms = NOW + 10;
    let first = f.journal.send_message(&f.a, &expiring, NOW).unwrap();
    let second = f.journal.send_message(&f.a, &mail("second"), NOW).unwrap();
    assert_eq!(
        f.journal
            .ack_message(&f.b, &second.message_id, NOW)
            .unwrap()
            .sequence,
        0
    );
    assert!(f.journal.inbox(&f.b, 10, NOW + 10).unwrap().is_empty());
    assert_eq!(
        f.journal.mailbox_cursor(&f.b, NOW + 10).unwrap().sequence,
        second.sequence
    );
    assert_eq!(
        f.journal.send_message(&f.a, &expiring, NOW + 10).unwrap(),
        first,
        "expired duplicate returns original durable sender receipt"
    );
    let pending = f.journal.send_message(&f.a, &mail("skip"), NOW).unwrap();
    assert_eq!(
        f.journal
            .skip_message(&f.owner, &pending.message_id, "handoff superseded", NOW)
            .unwrap()
            .sequence,
        pending.sequence
    );
}

#[test]
fn informational_supersession_never_discards_conversations_or_other_authors() {
    let mut f = fixture();
    let first = f.journal.send_message(&f.a, &mail("info"), NOW).unwrap();
    let mut question = mail("question");
    question.intent = MessageIntent::Question;
    let question = f.journal.send_message(&f.a, &question, NOW).unwrap();
    let mut replacement = mail("replacement");
    replacement.supersedes = Some(first.message_id.clone());
    assert!(f.journal.send_message(&f.c, &replacement, NOW).is_err());
    let latest = f.journal.send_message(&f.a, &replacement, NOW).unwrap();
    assert_eq!(
        f.journal.mailbox_cursor(&f.b, NOW).unwrap().sequence,
        first.sequence
    );
    f.journal
        .ack_message(&f.b, &latest.message_id, NOW)
        .unwrap();
    assert_eq!(
        f.journal.mailbox_cursor(&f.b, NOW).unwrap().sequence,
        first.sequence
    );
    replacement.idempotency_key = "cannot-drop-question".into();
    replacement.supersedes = Some(question.message_id.clone());
    assert!(f.journal.send_message(&f.a, &replacement, NOW).is_err());
    assert_eq!(
        f.journal
            .ack_message(&f.b, &question.message_id, NOW)
            .unwrap()
            .sequence,
        latest.sequence
    );
}

#[test]
fn bounded_mailbox_counts_bytes_and_expired_queues_release_capacity() {
    let mut f = fixture();
    for i in 0..256 {
        f.journal
            .send_message(&f.a, &mail(&format!("m{i}")), NOW)
            .unwrap();
    }
    assert!(f
        .journal
        .send_message(&f.a, &mail("overflow"), NOW)
        .unwrap_err()
        .to_string()
        .contains("full"));
    assert_eq!(f.journal.inbox(&f.b, 256, NOW).unwrap().len(), 256);
    let mut large = mail("oversize");
    large.body = Value::String("x".repeat(64 * 1024));
    assert!(f.journal.send_message(&f.a, &large, NOW).is_err());
    assert!(f.journal.inbox(&f.b, 256, NOW + 10000).unwrap().is_empty());
    let mut fresh = mail("capacity-returned");
    fresh.expires_at_ms = NOW + 20000;
    f.journal.send_message(&f.a, &fresh, NOW + 10000).unwrap();
    // A second mailbox hits the byte bound long before the 256-message bound.
    for i in 0..80 {
        let mut request = mail(&format!("large{i}"));
        request.recipient_run = "c".into();
        request.body = Value::String("x".repeat(60 * 1024));
        if f.journal.send_message(&f.a, &request, NOW).is_err() {
            assert!(i < 75 && i > 60);
            return;
        }
    }
    panic!("byte quota was not enforced");
}

#[test]
fn decision_acceptance_is_idempotent_versioned_and_delegated() {
    let mut f = fixture();
    let first = f
        .journal
        .accept_decision(&f.a, &decision("one", 0), NOW)
        .unwrap();
    assert_eq!(
        f.journal
            .accept_decision(&f.a, &decision("one", 0), NOW)
            .unwrap(),
        first
    );
    assert!(f
        .journal
        .accept_decision(&f.b, &decision("conflict", 0), NOW)
        .is_err());
    let second = f
        .journal
        .accept_decision(&f.b, &decision("two", 1), NOW)
        .unwrap();
    assert_eq!(second.supersedes, Some(first.decision_id));
    assert_eq!(second.version, 2);
    assert_eq!(
        f.journal
            .topic_versions("p", &set(&["api", "policy:build"]))
            .unwrap(),
        [("api".into(), 2), ("policy:build".into(), 0)]
            .into_iter()
            .collect()
    );
}

#[test]
fn two_connections_cannot_accept_incompatible_same_topic_version() {
    let f = fixture();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let path = f.path.clone();
            let token = f.a.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut j = Journal::open(path).unwrap();
                barrier.wait();
                j.accept_decision(&token, &decision(&format!("r{i}"), 0), NOW)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        f.journal
            .current_decision("p", "api")
            .unwrap()
            .unwrap()
            .version,
        1
    );
}

#[test]
fn final_integration_checks_base_owner_relevant_versions_and_exact_roots() {
    let mut f = fixture();
    let original = integration("landing", "new-head");
    for field in ["head", "source", "environment", "result-kind"] {
        let mut invalid = original.clone();
        match field {
            "head" => invalid.evidence.tested_head = "another-base".into(),
            "source" => {
                invalid.evidence.tested_source_root =
                    ObjectRef::from_bytes(ObjectKind::SourceRoot, b"wrong tree")
            }
            "environment" => {
                invalid.evidence.tested_environment_root =
                    ObjectRef::from_bytes(ObjectKind::EnvironmentRoot, b"wrong dependencies")
            }
            _ => invalid.evidence.action_results[0].kind = ObjectKind::Blob,
        }
        assert!(f.journal.integrate(&f.owner, &invalid, NOW).is_err());
    }
    let mut build = decision("build-policy", 0);
    build.topic = "policy:build".into();
    f.journal.accept_decision(&f.a, &build, NOW).unwrap();
    assert!(f.journal.integrate(&f.owner, &original, NOW).is_err());
    let mut current = original.clone();
    current.versions.insert("policy:build".into(), 1);
    let receipt = f.journal.integrate(&f.owner, &current, NOW).unwrap();
    assert_eq!(receipt.new_head, "new-head");
    assert_eq!(
        f.journal.integrate(&f.owner, &current, NOW).unwrap(),
        receipt
    );
    assert_eq!(
        f.journal.project_state("p").unwrap().integration_head,
        "new-head"
    );
    current.evidence.checks = json!({"conflicting":true});
    assert!(f.journal.integrate(&f.owner, &current, NOW).is_err());
}

#[test]
fn irrelevant_topic_updates_do_not_block_integration_but_owner_changes_do() {
    let mut f = fixture();
    let mut request = integration("relevant", "new");
    request.versions.remove("api");
    f.journal
        .accept_decision(&f.a, &decision("irrelevant", 0), NOW)
        .unwrap();
    let next = f
        .journal
        .transfer_owner(&f.owner, "new-owner", NOW)
        .unwrap();
    assert!(f.journal.integrate(&f.owner, &request, NOW).is_err());
    f.journal.integrate(&next, &request, NOW).unwrap();
}

#[test]
fn integration_head_race_has_one_commit_and_one_reverification_failure() {
    let f = fixture();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let path = f.path.clone();
            let owner = f.owner.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut j = Journal::open(path).unwrap();
                barrier.wait();
                j.integrate(
                    &owner,
                    &integration(&format!("r{i}"), &format!("head{i}")),
                    NOW,
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(f
        .journal
        .project_state("p")
        .unwrap()
        .integration_head
        .starts_with("head"));
}

#[test]
fn decision_invalidation_racing_integration_is_serialized_at_final_commit() {
    let f = fixture();
    let barrier = Arc::new(Barrier::new(2));
    let path = f.path.clone();
    let token = f.a.clone();
    let ready = barrier.clone();
    let changed = std::thread::spawn(move || {
        let mut j = Journal::open(path).unwrap();
        ready.wait();
        j.accept_decision(&token, &decision("invalidate", 0), NOW)
            .unwrap()
    });
    let path = f.path.clone();
    let owner = f.owner.clone();
    let ready = barrier.clone();
    let landed = std::thread::spawn(move || {
        let mut j = Journal::open(path).unwrap();
        ready.wait();
        j.integrate(&owner, &integration("land", "candidate"), NOW)
    });
    let changed = changed.join().unwrap();
    match landed.join().unwrap() {
        Ok(receipt) => assert!(
            receipt.sequence < changed.sequence,
            "old decision evidence can only commit before invalidation"
        ),
        Err(_) => assert_eq!(
            f.journal.project_state("p").unwrap().integration_head,
            "base"
        ),
    }
}

#[test]
fn command_utf8_byte_quota_is_bounded_and_old_epoch_backlog_does_not_stall_new_attempt() {
    let mut f = fixture();
    for index in 0..17 {
        let mut request = command(&format!("unicode-{index}"));
        request.payload = json!({"text":"🔥".repeat(60_000),"expected_thread_id":"thread-1","expected_turn_id":"turn-1"});
        f.journal.accept_command(&f.a, &request, NOW).unwrap();
    }
    let mut overflow = command("too-many-bytes");
    overflow.payload = json!({"text":"🔥".repeat(60_000),"expected_thread_id":"thread-1","expected_turn_id":"turn-1"});
    assert!(f
        .journal
        .accept_command(&f.a, &overflow, NOW)
        .unwrap_err()
        .to_string()
        .contains("full"));
    f.journal.advance_run_epoch(&f.owner, "b", 1, NOW).unwrap();
    working_runtime(&mut f.journal, &f.owner, "b", NOW);
    let mut rebound = grant("a", 1);
    rebound.recipients.insert("b".into(), 2);
    let token = f.journal.issue_capability(&f.owner, &rebound, NOW).unwrap();
    let mut fresh = command("new-epoch");
    fresh.target_epoch = 2;
    let accepted = f.journal.accept_command(&token, &fresh, NOW).unwrap();
    assert!(f
        .journal
        .claim_effect(&f.owner, &accepted.command_id, NOW)
        .unwrap()
        .is_some());
}

#[test]
fn sqlite_integer_limits_fail_atomically_without_real_epoch_promotion() {
    let mut f = fixture();
    let before = f.journal.project_state("p").unwrap();
    let db = rusqlite::Connection::open(&f.path).unwrap();
    db.execute(
        "UPDATE runs SET epoch=9223372036854775807 WHERE project_id='p' AND run_id='b'",
        [],
    )
    .unwrap();
    assert!(f.journal.transfer_owner(&f.owner, "next", NOW).is_err());
    assert_eq!(f.journal.project_state("p").unwrap(), before);
    assert!(f
        .journal
        .advance_run_epoch(&f.owner, "b", i64::MAX as u64, NOW)
        .is_err());
    assert!(f
        .journal
        .advance_run_epoch(&f.owner, "b", u64::MAX, NOW)
        .is_err());
    db.execute(
        "UPDATE projects SET event_seq=9223372036854775807 WHERE project_id='p'",
        [],
    )
    .unwrap();
    assert!(f.journal.register_run(&f.owner, "overflow", NOW).is_err());
    assert!(f.journal.run_epoch("p", "overflow").is_err());
}

#[test]
fn observed_instance_changes_fence_same_engine_resumes_but_heartbeats_do_not() {
    let mut f = fixture();
    assert_eq!(
        f.journal
            .observe_run_attempt(&f.owner, "a", "pty-1", NOW)
            .unwrap(),
        1
    );
    let sequence = f.journal.project_state("p").unwrap().sequence;
    assert_eq!(
        f.journal
            .observe_run_attempt(&f.owner, "a", "pty-1", NOW)
            .unwrap(),
        1
    );
    assert_eq!(f.journal.project_state("p").unwrap().sequence, sequence);
    assert_eq!(
        f.journal
            .observe_run_attempt(&f.owner, "a", "pty-2", NOW)
            .unwrap(),
        2
    );
    assert!(f.journal.find_agents(&f.a, NOW).is_err());
    assert_eq!(
        f.journal
            .observe_run_attempt(&f.owner, "new-run", "new-pty", NOW)
            .unwrap(),
        1
    );
}

#[test]
fn durable_journal_rejects_memory_uris_and_is_private_on_unix() {
    for path in ["", ":memory:", "file:mem?mode=memory&cache=shared"] {
        assert!(Journal::open(path).is_err());
    }
    let f = fixture();
    let db = rusqlite::Connection::open(&f.path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert!(rusqlite::version_number() >= 3_051_003);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&f.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn authenticated_identity_receipts_and_topics_are_bound_to_exact_sender_attempt() {
    let mut f = fixture();
    let identity = f.journal.authenticate_agent(&f.a, NOW).unwrap();
    assert_eq!(identity.project_id, "p");
    assert_eq!(identity.run_id, "a");
    assert_eq!(identity.owner_epoch, 1);
    let receipt = f
        .journal
        .accept_command(&f.a, &command("scoped-status"), NOW)
        .unwrap();
    assert_eq!(
        f.journal
            .agent_command_receipt(&f.a, &receipt.command_id, NOW)
            .unwrap(),
        receipt
    );
    assert!(f
        .journal
        .agent_command_receipt(&f.b, &receipt.command_id, NOW)
        .is_err());
    f.journal
        .accept_decision(&f.a, &decision("accepted", 0), NOW)
        .unwrap();
    let topics = f
        .journal
        .read_topics(&f.a, &set(&["api", "policy:build"]), NOW)
        .unwrap();
    assert_eq!(topics["api"].as_ref().unwrap().version, 1);
    assert!(topics["policy:build"].is_none());
    assert!(f
        .journal
        .read_topics(&f.a, &set(&["secret-topic"]), NOW)
        .is_err());
    f.journal.advance_run_epoch(&f.owner, "a", 1, NOW).unwrap();
    let mut replacement = grant("a", 2);
    replacement.recipients.insert("b".into(), 1);
    replacement.recipients.insert("c".into(), 1);
    let fresh = f
        .journal
        .issue_capability(&f.owner, &replacement, NOW)
        .unwrap();
    assert!(f
        .journal
        .agent_command_receipt(&fresh, &receipt.command_id, NOW)
        .is_err());
    assert!(
        f.journal
            .accept_command(&fresh, &command("scoped-status"), NOW)
            .is_err(),
        "idempotency cannot adopt another sender attempt's receipt"
    );
}

#[test]
fn ready_projection_cannot_be_starved_by_stale_or_blocked_recovery_rows() {
    let mut f = fixture();
    let first = f
        .journal
        .accept_command(&f.a, &command("uncertain-head"), NOW)
        .unwrap();
    f.journal
        .claim_effect(&f.owner, &first.command_id, NOW)
        .unwrap()
        .unwrap();
    f.journal.recover(&f.owner, NOW).unwrap();
    for i in 0..20 {
        f.journal
            .accept_command(&f.a, &command(&format!("blocked-{i}")), NOW)
            .unwrap();
    }
    let mut other = command("other-target");
    other.target_run = "c".into();
    let ready = f.journal.accept_command(&f.a, &other, NOW).unwrap();
    assert_eq!(f.journal.ready_commands(&f.owner, 8).unwrap(), vec![ready]);
    f.journal.advance_run_epoch(&f.owner, "b", 1, NOW).unwrap();
    working_runtime(&mut f.journal, &f.owner, "b", NOW);
    let mut rebound = grant("a", 1);
    rebound.recipients.insert("b".into(), 2);
    let token = f.journal.issue_capability(&f.owner, &rebound, NOW).unwrap();
    let mut new = command("new-epoch-ready");
    new.target_epoch = 2;
    let fresh = f.journal.accept_command(&token, &new, NOW).unwrap();
    assert!(f
        .journal
        .ready_commands(&f.owner, 8)
        .unwrap()
        .contains(&fresh));
    assert_eq!(
        f.journal
            .commands_in_state(&f.owner, CommandState::Uncertain, 100)
            .unwrap()
            .len(),
        1,
        "old uncertain history remains available for reconciliation"
    );
}

#[test]
fn command_receipts_survive_target_replacement_without_replaying_effects() {
    for state in [CommandState::Accepted, CommandState::Started, CommandState::Uncertain, CommandState::Completed] {
        let mut f = fixture();
        let request = command("historical-receipt");
        let accepted = f.journal.accept_command(&f.a, &request, NOW).unwrap();
        if state != CommandState::Accepted {
            let claim = f.journal.claim_effect(&f.owner, &accepted.command_id, NOW).unwrap().unwrap();
            if state == CommandState::Completed {
                f.journal.finish_effect(&claim, &json!({"status":"interrupted","turn_id":"original-turn"}), NOW).unwrap();
            } else if state == CommandState::Uncertain {
                f.journal.recover(&f.owner, NOW).unwrap();
            }
        }
        let before_retirement = f.journal.command_receipt(&f.owner, &accepted.command_id).unwrap();
        assert_eq!(before_retirement.state, state);
        f.journal.advance_run_epoch(&f.owner, "b", 1, NOW).unwrap();
        working_runtime(&mut f.journal, &f.owner, "b", NOW);
        let original = f.journal.command_receipt(&f.owner, &accepted.command_id).unwrap();
        assert_eq!(original.command_id, before_retirement.command_id);
        assert_eq!(original.accepted_sequence, before_retirement.accepted_sequence);
        if state == CommandState::Accepted {
            assert_eq!(original.state, CommandState::Completed);
            assert_eq!(original.result.as_ref().unwrap()["status"], "rejected");
        } else { assert_eq!(original, before_retirement); }
        let mut replacement = grant("a", 1);
        replacement.recipients.insert("b".into(), 2);
        let fresh_token = f.journal.issue_capability(&f.owner, &replacement, NOW).unwrap();
        let db = rusqlite::Connection::open(&f.path).unwrap();
        let count = || db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_,i64>(0)).unwrap();
        let before = count();
        for token in [&f.a, &fresh_token] {
            assert_eq!(f.journal.accept_command(token, &request, NOW).unwrap(), original);
            assert_eq!(f.journal.agent_command_receipt(token, &original.command_id, NOW).unwrap(), original);
        }
        assert_eq!(count(), before, "historical lookup must not enqueue or replay an effect");
        assert!(f.journal.claim_effect(&f.owner, &original.command_id, NOW).is_err(), "the old effect cannot be claimed for a new target attempt");
        let mut new = command("new-command");
        assert!(f.journal.accept_command(&fresh_token, &new, NOW).is_err());
        new.target_epoch = 2;
        assert!(f.journal.accept_command(&fresh_token, &new, NOW).is_ok());
        let mut changed = request.clone();
        changed.target_epoch = 2;
        assert!(f.journal.accept_command(&fresh_token, &changed, NOW).is_err());
        f.journal.revoke_capability(&f.owner, &f.a, NOW).unwrap();
        assert!(f.journal.agent_command_receipt(&f.a, &original.command_id, NOW).is_err());
        f.journal.advance_run_epoch(&f.owner, "a", 1, NOW).unwrap();
        replacement.run_epoch = 2;
        replacement.recipients.insert("a".into(), 2);
        let next_sender = f.journal.issue_capability(&f.owner, &replacement, NOW).unwrap();
        assert!(f.journal.agent_command_receipt(&next_sender, &original.command_id, NOW).is_err());
        assert!(f.journal.accept_command(&next_sender, &request, NOW).is_err());
    }
}

#[test]
fn sender_message_receipts_do_not_reroute_into_a_replacement_recipient() {
    let mut f = fixture();
    let request = mail("historical-message");
    let original = f.journal.send_message_with_ttl(&f.a, &request, 10_000, NOW).unwrap();
    f.journal.advance_run_epoch(&f.owner, "b", 1, NOW).unwrap();
    working_runtime(&mut f.journal, &f.owner, "b", NOW);
    let mut replacement = grant("a", 1);
    replacement.recipients.insert("b".into(), 2);
    let token = f.journal.issue_capability(&f.owner, &replacement, NOW).unwrap();
    for sender in [&f.a, &token] {
        assert_eq!(f.journal.sent_message(sender, &request.idempotency_key, NOW+100).unwrap(), Some(original.clone()));
        assert_eq!(f.journal.send_message_with_ttl(sender, &request, 10_000, NOW+100).unwrap(), original);
    }
    let b = f.journal.issue_capability(&f.owner, &CapabilityGrant { run_epoch:2, recipients: replacement.recipients.clone(), ..grant("b",2) }, NOW).unwrap();
    assert!(f.journal.inbox(&b, 100, NOW).unwrap().is_empty(), "historical messages remain addressed to the original attempt");
    let mut new = request.clone();
    new.idempotency_key = "new-message".into();
    assert!(f.journal.send_message(&token, &new, NOW).is_err());
    new.recipient_epoch = 2;
    assert!(f.journal.send_message(&token, &new, NOW).is_ok());
    let mut changed = request.clone();
    changed.recipient_epoch = 2;
    assert!(f.journal.send_message_with_ttl(&token, &changed, 10_000, NOW).is_err());
    f.journal.advance_run_epoch(&f.owner, "a", 1, NOW).unwrap();
    replacement.run_epoch=2;
    replacement.recipients.insert("a".into(),2);
    let next_sender = f.journal.issue_capability(&f.owner, &replacement, NOW).unwrap();
    assert!(f.journal.sent_message(&next_sender, &request.idempotency_key, NOW).is_err());
}

#[test]
fn live_controls_are_rejected_before_dispatch_after_grant_sender_or_turn_changes() {
    for change in ["grant", "sender", "turn"] {
        let mut f=fixture();
        let request=command("pending-control");
        let accepted=f.journal.accept_command(&f.a,&request,NOW).unwrap();
        match change {
            "grant" => f.journal.revoke_capability(&f.owner,&f.a,NOW).unwrap(),
            "sender" => { f.journal.advance_run_epoch(&f.owner,"a",1,NOW).unwrap(); },
            "turn" => {
                let mut observation=f.journal.runtime_snapshot(&f.owner,"b",NOW).unwrap().unwrap().observation;
                observation.active_turn_id=Some("new-turn".into());
                f.journal.observe_runtime(&f.owner,&observation,NOW).unwrap();
            },
            _=>unreachable!(),
        }
        assert!(f.journal.claim_effect(&f.owner,&accepted.command_id,NOW).unwrap().is_none(),"no effect starts after {change} changed");
        let rejected=f.journal.command_receipt(&f.owner,&accepted.command_id).unwrap();
        assert_eq!(rejected.state,CommandState::Completed);
        assert_eq!(rejected.result.as_ref().unwrap()["status"],"rejected");
        let db=rusqlite::Connection::open(&f.path).unwrap();
        assert_eq!(db.query_row("SELECT count(*) FROM events WHERE kind='effect-started'",[],|row|row.get::<_,i64>(0)).unwrap(),0);
        if change=="turn" {
            assert_eq!(f.journal.accept_command(&f.a,&request,NOW).unwrap(),rejected,"a retry returns its old outcome without binding to the new turn");
            let mut next=command("new-control");next.payload["expected_turn_id"]=json!("new-turn");
            let fresh=f.journal.accept_command(&f.a,&next,NOW).unwrap();
            assert!(f.journal.claim_effect(&f.owner,&fresh.command_id,NOW).unwrap().is_some());
        }
    }
}

#[test]
fn schema_one_migration_preserves_receipts_and_rejects_unproven_legacy_live_effects() {
    let mut f=fixture();
    let request=command("legacy-command");
    let accepted=f.journal.accept_command(&f.a,&request,NOW).unwrap();
    drop(f.journal);
    let db=rusqlite::Connection::open(&f.path).unwrap();
    db.execute_batch("DROP TABLE runtime_observations; ALTER TABLE commands DROP COLUMN capability_hash; PRAGMA user_version=1;").unwrap();
    drop(db);
    let mut journal=Journal::open(&f.path).unwrap();
    assert_eq!(journal.agent_command_receipt(&f.a,&accepted.command_id,NOW).unwrap(),accepted);
    assert_eq!(journal.accept_command(&f.a,&request,NOW).unwrap(),accepted);
    working_runtime(&mut journal,&f.owner,"b",NOW);
    assert!(journal.claim_effect(&f.owner,&accepted.command_id,NOW).unwrap().is_none());
    let rejected=journal.command_receipt(&f.owner,&accepted.command_id).unwrap();
    assert_eq!(rejected.result.unwrap()["status"],"rejected");
    let reopened=Journal::open(&f.path).unwrap();
    assert_eq!(reopened.project_state("p").unwrap().owner,f.owner);
}
