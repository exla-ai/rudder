//! Real filesystem SQLite/transactions and local socket bytes. Backend/model
//! interpretation and general process-tree settlement are separate checks.
use super::*;
use tempfile::TempDir;

const NOW: i64 = 1_000;
struct Fixture {
    temporary: TempDir,
    journal: Journal,
    owner: OwnerFence,
    sender: String,
    receiver: String,
}
fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| (*v).into()).collect()
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(temporary.path().join("journal.sqlite")).unwrap();
        let owner = journal.create_project("p", "engine", "base", NOW).unwrap();
        for run in ["a", "b"] {
            journal.register_run(&owner, run, NOW).unwrap();
        }
        journal
            .observe_run_attempt(&owner, "b", "attempt-b", NOW)
            .unwrap();
        let grant = |run: &str| CapabilityGrant {
            principal: format!("principal-{run}"),
            run_id: run.into(),
            run_epoch: 1,
            operations: set(&["message:send", "mailbox:read", "mailbox:ack"]),
            recipients: BTreeMap::from([("a".into(), 1), ("b".into(), 1)]),
            scopes: set(&["project"]),
            topics: BTreeSet::new(),
            expires_at_ms: None,
        };
        let sender = journal.issue_capability(&owner, &grant("a"), NOW).unwrap();
        let receiver = journal.issue_capability(&owner, &grant("b"), NOW).unwrap();
        let mut value = Self {
            temporary,
            journal,
            owner,
            sender,
            receiver,
        };
        value.observe(RuntimePhase::Waiting, true, "thread", NOW);
        value
    }
    fn observe(&mut self, phase: RuntimePhase, deliver: bool, thread: &str, now: i64) {
        self.journal
            .observe_runtime(
                &self.owner,
                &RuntimeObservation {
                    run_id: "b".into(),
                    run_epoch: 1,
                    attempt_id: "attempt-b".into(),
                    backend: "codex".into(),
                    execution_profile: "codex-app-server".into(),
                    thread_id: Some(thread.into()),
                    active_turn_id: (phase == RuntimePhase::Working).then(|| "busy-turn".into()),
                    last_interrupted_turn_id: (phase == RuntimePhase::Interrupted)
                        .then(|| "interrupted-turn".into()),
                    phase,
                    capabilities: if deliver {
                        set(&["inspect", "deliver"])
                    } else {
                        set(&["inspect"])
                    },
                    pty_owned: true,
                },
                now,
            )
            .unwrap();
    }
    fn send(&mut self, key: &str) -> MessageEnvelope {
        self.journal
            .send_message(
                &self.sender,
                &MessageRequest {
                    idempotency_key: key.into(),
                    recipient_run: "b".into(),
                    recipient_epoch: 1,
                    scope: "project".into(),
                    intent: MessageIntent::Inform,
                    body: serde_json::json!({"context":"peer data"}),
                    expires_at_ms: NOW + 60_000,
                    priority: MessagePriority::Normal,
                    conversation_id: None,
                    request_id: None,
                    reply_to: None,
                    supersedes: None,
                    topic_versions: BTreeMap::new(),
                    artifacts: vec![],
                },
                NOW,
            )
            .unwrap()
    }
    fn claim(&mut self, now: i64) -> Option<(EffectClaim, i64)> {
        self.journal
            .claim_idle_mailbox_delivery(&self.owner, "b", "attempt-b", now)
            .unwrap()
            .map(ClaimedMailboxDelivery::into_effect)
    }
    fn commands(&self) -> i64 {
        self.journal
            .conn
            .query_row("SELECT count(*) FROM commands", [], |r| r.get(0))
            .unwrap()
    }
}

#[test]
fn automatic_idle_mail_starts_once_and_transport_acceptance_never_acknowledges() {
    let mut f = Fixture::new();
    let mail = f.send("one");
    let (claim, accepted) = f.claim(NOW + 1).unwrap();
    assert_eq!(accepted, NOW + 1);
    assert_eq!(claim.kind, "deliver");
    assert_eq!(claim.principal, mail.author);
    assert_eq!(claim.sender_run, mail.sender_run);
    assert_eq!(claim.sender_epoch, mail.sender_epoch);
    assert_eq!(
        claim.payload["mailbox_notice"],
        serde_json::to_value(MailboxDeliveryNotice::from_message(&mail)).unwrap()
    );
    assert_eq!(claim.payload["expected_attempt_id"], "attempt-b");
    assert_eq!(claim.payload["expected_thread_id"], "thread");
    assert!(claim.payload["expected_turn_id"].is_null());
    assert_eq!(f.commands(), 1);
    assert!(f.claim(NOW + 2).is_none());
    f.journal
        .finish_effect(
            &claim,
            &serde_json::json!({"adapter":{"status":"transport_accepted","turn_id":"new-turn"}}),
            NOW + 3,
        )
        .unwrap();
    assert!(f.claim(NOW + 4).is_none());
    assert_eq!(f.commands(), 1);
    assert_eq!(
        f.journal.inbox(&f.receiver, 10, NOW + 4).unwrap(),
        vec![mail.clone()]
    );
    assert_eq!(
        f.journal
            .mailbox_cursor(&f.receiver, NOW + 4)
            .unwrap()
            .sequence,
        0
    );
    f.journal
        .ack_message(&f.receiver, &mail.message_id, NOW + 5)
        .unwrap();
    assert_eq!(
        f.journal
            .mailbox_cursor(&f.receiver, NOW + 5)
            .unwrap()
            .sequence,
        mail.sequence
    );
}

#[test]
fn automatic_busy_interrupted_failed_closed_uncertain_and_unqualified_mail_stays_queued() {
    let mut f = Fixture::new();
    let mail = f.send("queued");
    for phase in [
        RuntimePhase::Working,
        RuntimePhase::Interrupted,
        RuntimePhase::Failed,
        RuntimePhase::Closed,
        RuntimePhase::Uncertain,
        RuntimePhase::Starting,
    ] {
        f.observe(phase, false, "thread", NOW + 1);
        assert!(f.claim(NOW + 2).is_none());
        assert_eq!(f.commands(), 0);
    }
    f.observe(RuntimePhase::Waiting, false, "thread", NOW + 3);
    assert!(f.claim(NOW + 4).is_none());
    assert_eq!(
        f.journal.inbox(&f.receiver, 10, NOW + 5).unwrap(),
        vec![mail]
    );
    f.observe(RuntimePhase::Completed, true, "thread", NOW + 6);
    assert!(f.claim(NOW + 7).is_some());
}

#[test]
fn automatic_stale_observation_wrong_attempt_and_replaced_epoch_do_not_route() {
    let mut f = Fixture::new();
    f.send("queued");
    assert!(f.claim(NOW + RUNTIME_MAX_AGE_MS + 1).is_none());
    assert_eq!(f.commands(), 0);
    f.observe(RuntimePhase::Waiting, true, "thread", NOW + 10);
    assert!(f
        .journal
        .claim_idle_mailbox_delivery(&f.owner, "b", "different-attempt", NOW + 11)
        .unwrap()
        .is_none());
    f.journal
        .observe_run_attempt(&f.owner, "b", "replacement-attempt", NOW + 12)
        .unwrap();
    assert!(f
        .journal
        .claim_idle_mailbox_delivery(&f.owner, "b", "replacement-attempt", NOW + 13)
        .unwrap()
        .is_none());
    assert_eq!(f.commands(), 0);
}

#[test]
fn automatic_claim_transaction_failure_rolls_back_before_any_external_write() {
    let mut f = Fixture::new();
    let mail = f.send("atomic");
    f.journal.conn.execute_batch("CREATE TRIGGER refuse_effect BEFORE INSERT ON effects BEGIN SELECT RAISE(ABORT,'fixture ledger fault'); END;").unwrap();
    assert!(f
        .journal
        .claim_idle_mailbox_delivery(&f.owner, "b", "attempt-b", NOW + 1)
        .is_err());
    assert_eq!(f.commands(), 0);
    assert_eq!(
        f.journal.inbox(&f.receiver, 10, NOW + 2).unwrap(),
        vec![mail]
    );
    f.journal
        .conn
        .execute_batch("DROP TRIGGER refuse_effect")
        .unwrap();
    assert!(f.claim(NOW + 3).is_some());
}

#[cfg(unix)]
#[test]
fn automatic_actual_socket_write_lost_receipt_and_reopen_never_replay() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let mut f = Fixture::new();
    let mail = f.send("uncertain");
    let (claim, _) = f.claim(NOW + 1).unwrap();
    let (mut writer, mut reader) = UnixStream::pair().unwrap();
    let bytes = crate::protocol::canonical_json(&claim.payload).unwrap();
    writer.write_all(&bytes).unwrap();
    writer.shutdown(std::net::Shutdown::Write).unwrap();
    let mut actual = Vec::new();
    reader.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, bytes);
    assert_eq!(
        serde_json::from_slice::<Value>(&actual).unwrap()["mailbox_notice"]["message_id"],
        mail.message_id
    );
    // Crash/lost reply AFTER actual bytes, without marking completion. A new
    // opening/recovering observer sees original uncertainty, never a fresh send.
    let path = f.temporary.path().join("journal.sqlite");
    drop(f.journal);
    f.journal = Journal::open(path).unwrap();
    let recovery = f.journal.recover(&f.owner, NOW + 2).unwrap();
    assert_eq!(recovery.uncertain.len(), 1);
    assert_eq!(recovery.uncertain[0].command_id, claim.command_id);
    assert!(f.claim(NOW + 3).is_none());
    assert_eq!(f.commands(), 1);
    assert_eq!(
        f.journal.inbox(&f.receiver, 10, NOW + 4).unwrap(),
        vec![mail]
    );
    assert_eq!(
        f.journal
            .mailbox_cursor(&f.receiver, NOW + 4)
            .unwrap()
            .sequence,
        0
    );
}

#[test]
fn automatic_later_notification_does_not_jump_an_acknowledgment_hole() {
    let mut f = Fixture::new();
    let first = f.send("first");
    let second = f.send("second");
    let (one, _) = f.claim(NOW + 1).unwrap();
    f.journal
        .finish_effect(
            &one,
            &serde_json::json!({"transport_accepted":true}),
            NOW + 2,
        )
        .unwrap();
    let (two, _) = f.claim(NOW + 3).unwrap();
    assert_eq!(
        two.payload["mailbox_notice"]["message_id"],
        second.message_id
    );
    f.journal
        .finish_effect(
            &two,
            &serde_json::json!({"transport_accepted":true}),
            NOW + 4,
        )
        .unwrap();
    f.journal
        .ack_message(&f.receiver, &second.message_id, NOW + 5)
        .unwrap();
    assert_eq!(
        f.journal
            .mailbox_cursor(&f.receiver, NOW + 5)
            .unwrap()
            .sequence,
        0
    );
    f.journal
        .ack_message(&f.receiver, &first.message_id, NOW + 6)
        .unwrap();
    assert_eq!(
        f.journal
            .mailbox_cursor(&f.receiver, NOW + 6)
            .unwrap()
            .sequence,
        second.sequence
    );
}

#[test]
fn automatic_expired_or_acknowledged_mail_never_starts_a_notification() {
    let mut f = Fixture::new();
    let first = f.send("acked");
    f.journal
        .ack_message(&f.receiver, &first.message_id, NOW + 1)
        .unwrap();
    assert!(f.claim(NOW + 2).is_none());
    f.send("expired");
    f.observe(RuntimePhase::Waiting, true, "thread", NOW + 60_001);
    assert!(f.claim(NOW + 60_002).is_none());
    assert_eq!(f.commands(), 0);
}

#[test]
fn automatic_recovery_cannot_retarget_original_thread_or_refresh_its_deadline() {
    let mut f = Fixture::new();
    f.send("identity");
    let (claim, _) = f.claim(NOW + 1).unwrap();
    f.journal
        .mark_effect_uncertain(&claim, &serde_json::json!({"fixture":"no result"}), NOW + 2)
        .unwrap();
    f.journal
        .reconcile_effect(
            &f.owner,
            &claim.command_id,
            &Reconciliation::RetrySafe {
                evidence: serde_json::json!({"fixture":"known no write"}),
            },
            NOW + 3,
        )
        .unwrap();
    f.observe(RuntimePhase::Waiting, true, "new-thread", NOW + 4);
    assert!(f
        .journal
        .claim_effect(&f.owner, &claim.command_id, NOW + 5)
        .unwrap()
        .is_none());
    assert_eq!(
        f.journal
            .command_receipt(&f.owner, &claim.command_id)
            .unwrap()
            .state,
        CommandState::Completed
    );
    assert!(f.claim(NOW + 6).is_none());
    let mut f = Fixture::new();
    f.send("timeout");
    let (claim, _) = f.claim(NOW + 1).unwrap();
    f.journal
        .mark_effect_uncertain(&claim, &serde_json::json!({"fixture":"no result"}), NOW + 2)
        .unwrap();
    f.journal
        .reconcile_effect(
            &f.owner,
            &claim.command_id,
            &Reconciliation::RetrySafe {
                evidence: serde_json::json!({"fixture":"known no write"}),
            },
            NOW + 3,
        )
        .unwrap();
    f.observe(RuntimePhase::Waiting, true, "thread", NOW + 31_001);
    assert!(f
        .journal
        .claim_effect(&f.owner, &claim.command_id, NOW + 31_002)
        .unwrap()
        .is_none());
    assert!(f.claim(NOW + 31_003).is_none());
    assert_eq!(f.commands(), 1);
}

#[test]
fn automatic_agent_cannot_reserve_engine_delivery_key_or_supply_its_origin() {
    let mut f = Fixture::new();
    let mail = f.send("one");
    let forged = CommandRequest {
        idempotency_key: format!("{PREFIX}{}", mail.message_id),
        target_run: "b".into(),
        target_epoch: 1,
        kind: "deliver".into(),
        scope: "project".into(),
        payload: serde_json::json!({"principal":"engine"}),
    };
    assert!(f
        .journal
        .accept_command(&f.sender, &forged, NOW + 1)
        .is_err());
    assert_eq!(f.commands(), 0);
    let (claim, _) = f.claim(NOW + 2).unwrap();
    assert_eq!(claim.principal, "principal-a");
}

#[test]
fn automatic_owner_transfer_and_old_target_cannot_revive_historical_delivery() {
    let mut f = Fixture::new();
    f.send("old");
    let (claim, _) = f.claim(NOW + 1).unwrap();
    let old = f.owner.clone();
    f.owner = f
        .journal
        .transfer_owner(&old, "new-engine", NOW + 2)
        .unwrap();
    assert!(f
        .journal
        .claim_idle_mailbox_delivery(&old, "b", "attempt-b", NOW + 3)
        .is_err());
    assert!(f.claim(NOW + 4).is_none());
    assert_eq!(f.commands(), 1);
    assert!(matches!(
        f.journal
            .command_receipt(&f.owner, &claim.command_id)
            .unwrap()
            .state,
        CommandState::Started | CommandState::Uncertain
    ));
}

#[test]
fn automatic_notification_requires_only_accepted_mail_and_grants_no_live_peer_control() {
    let mut f = Fixture::new();
    f.send("informational");
    let manual = CommandRequest {
        idempotency_key: "manual-deliver".into(),
        target_run: "b".into(),
        target_epoch: 1,
        kind: "deliver".into(),
        scope: "project".into(),
        payload: serde_json::json!({"expected_thread_id":"thread","instruction":"ordinary peer"}),
    };
    assert!(f
        .journal
        .accept_command(&f.sender, &manual, NOW + 1)
        .is_err());
    let (claim, _) = f.claim(NOW + 2).unwrap();
    assert_eq!(claim.kind, "deliver");
    assert_eq!(claim.principal, "principal-a");
    for kind in ["steer", "interrupt", "resume"] {
        let control = CommandRequest {
            idempotency_key: format!("forbidden-{kind}"),
            kind: kind.into(),
            ..manual.clone()
        };
        assert!(f
            .journal
            .accept_command(&f.sender, &control, NOW + 3)
            .is_err());
    }
    assert_eq!(f.commands(), 1);
}

#[test]
fn automatic_notification_never_jumps_existing_explicit_or_uncertain_effects() {
    let mut f = Fixture::new();
    f.send("wait-for-control");
    let token = f
        .journal
        .issue_capability(
            &f.owner,
            &CapabilityGrant {
                principal: "principal-a".into(),
                run_id: "a".into(),
                run_epoch: 1,
                operations: set(&["command:deliver"]),
                recipients: BTreeMap::from([("b".into(), 1)]),
                scopes: set(&["project"]),
                topics: BTreeSet::new(),
                expires_at_ms: None,
            },
            NOW,
        )
        .unwrap();
    let request = CommandRequest {
        idempotency_key: "explicit".into(),
        target_run: "b".into(),
        target_epoch: 1,
        kind: "deliver".into(),
        scope: "project".into(),
        payload: serde_json::json!({"expected_thread_id":"thread","instruction":"explicit message"}),
    };
    let receipt = f.journal.accept_command(&token, &request, NOW + 1).unwrap();
    assert!(f.claim(NOW + 2).is_none());
    let claim = f
        .journal
        .claim_effect(&f.owner, &receipt.command_id, NOW + 3)
        .unwrap()
        .unwrap();
    f.journal
        .mark_effect_uncertain(
            &claim,
            &serde_json::json!({"fixture":"lost explicit receipt"}),
            NOW + 4,
        )
        .unwrap();
    assert!(f.claim(NOW + 5).is_none());
    f.journal
        .reconcile_effect(
            &f.owner,
            &receipt.command_id,
            &Reconciliation::Completed {
                result: serde_json::json!({"fixture":"known explicit outcome"}),
                evidence: serde_json::json!({"fixture":"actual matching response"}),
            },
            NOW + 6,
        )
        .unwrap();
    assert!(f.claim(NOW + 7).is_some());
}

#[test]
fn automatic_notification_ledger_does_not_duplicate_large_peer_payloads() {
    let mut f = Fixture::new();
    let mut request = f.send("template").request;
    request.idempotency_key = "large".into();
    request.priority = MessagePriority::Urgent;
    request.body = serde_json::json!({"text":"X".repeat(58_000)});
    let mail = f.journal.send_message(&f.sender, &request, NOW + 1).unwrap();
    let (claim, _) = f.claim(NOW + 2).unwrap();
    assert_eq!(claim.payload["mailbox_notice"]["message_id"], mail.message_id);
    let encoded = crate::protocol::canonical_json(&claim.payload).unwrap();
    assert!(encoded.len() < 2048);
    assert!(!String::from_utf8(encoded).unwrap().contains(&"X".repeat(100)));
    assert_eq!(f.journal.inbox(&f.receiver, 10, NOW + 3).unwrap()[0], mail);
}
