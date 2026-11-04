//! Production journal + production notification framing + production link over
//! real WebSocket bytes. The remote backend is the existing protocol fixture.
//! This does not claim a model ran or a tool/process tree was stopped.
use super::*;
use rudder_engine::journal::{
    CapabilityGrant, CommandState, EffectClaim, Journal, MessageEnvelope, MessageIntent,
    MessagePriority, MessageRequest, OwnerFence, RuntimeObservation, RuntimePhase,
};
use std::collections::{BTreeMap, BTreeSet};

const NOW: i64 = 1_000;
struct Mail {
    journal: Journal,
    owner: OwnerFence,
    receiver: String,
    message: MessageEnvelope,
    claim: EffectClaim,
}
impl Mail {
    fn new(server: &MockServer) -> Self {
        let mut journal = Journal::open(server._scratch.0.join("mail.sqlite")).unwrap();
        let owner = journal.create_project("p", "engine", "base", NOW).unwrap();
        for run in ["peer", "run-one"] {
            journal.register_run(&owner, run, NOW).unwrap();
        }
        journal
            .observe_run_attempt(&owner, "run-one", &server.launch.attempt_id, NOW)
            .unwrap();
        let grant = |run: &str| CapabilityGrant {
            principal: format!("principal-{run}"),
            run_id: run.into(),
            run_epoch: 1,
            operations: ["message:send", "mailbox:read", "mailbox:ack"]
                .into_iter()
                .map(String::from)
                .collect(),
            recipients: BTreeMap::from([("run-one".into(), 1)]),
            scopes: BTreeSet::from(["project".into()]),
            topics: BTreeSet::new(),
            expires_at_ms: None,
        };
        let sender = journal
            .issue_capability(&owner, &grant("peer"), NOW)
            .unwrap();
        let receiver = journal
            .issue_capability(&owner, &grant("run-one"), NOW)
            .unwrap();
        let message = journal
            .send_message(
                &sender,
                &MessageRequest {
                    idempotency_key: "one".into(),
                    recipient_run: "run-one".into(),
                    recipient_epoch: 1,
                    scope: "project".into(),
                    intent: MessageIntent::Question,
                    body: json!({"text":"Which interface should I use?"}),
                    expires_at_ms: NOW + 60_000,
                    priority: MessagePriority::Normal,
                    conversation_id: None,
                    request_id: Some("question".into()),
                    reply_to: None,
                    supersedes: None,
                    topic_versions: BTreeMap::new(),
                    artifacts: vec![],
                },
                NOW,
            )
            .unwrap();
        journal
            .observe_runtime(
                &owner,
                &RuntimeObservation {
                    run_id: "run-one".into(),
                    run_epoch: 1,
                    attempt_id: server.launch.attempt_id.clone(),
                    backend: "codex".into(),
                    execution_profile: "codex-app-server".into(),
                    thread_id: Some("thread-main".into()),
                    active_turn_id: None,
                    last_interrupted_turn_id: None,
                    phase: RuntimePhase::Waiting,
                    capabilities: BTreeSet::from(["inspect".into(), "deliver".into()]),
                    pty_owned: true,
                },
                NOW,
            )
            .unwrap();
        let (claim, _) = journal
            .claim_idle_mailbox_delivery(&owner, "run-one", &server.launch.attempt_id, NOW + 1)
            .unwrap()
            .unwrap()
            .into_effect();
        Self {
            journal,
            owner,
            receiver,
            message,
            claim,
        }
    }
    fn submit(&self, link: &AgentLinkHandle) {
        let message = crate::fabric_runtime::automatic_mailbox_message_for_test(&self.claim)
            .unwrap()
            .unwrap();
        let mut input = command(
            &self.claim.command_id,
            AgentLinkOperation::Deliver { message },
            None,
        );
        input.timeout = Duration::from_secs(2);
        link.submit(input).unwrap();
    }
    fn still_unacknowledged(&mut self) {
        assert_eq!(
            self.journal.inbox(&self.receiver, 10, NOW + 5).unwrap(),
            vec![self.message.clone()]
        );
        assert_eq!(
            self.journal
                .mailbox_cursor(&self.receiver, NOW + 5)
                .unwrap()
                .sequence,
            0
        );
    }
}

#[test]
fn automatic_mail_actual_websocket_wakes_original_idle_thread_and_never_acks() {
    let server = MockServer::new(None, false);
    let (mut link, events) = server.spawn();
    until(&events, |event| {
        matches!(event, AgentLinkEventKind::Bound { .. })
    });
    let mut mail = Mail::new(&server);
    mail.submit(&link);
    let request = server.next_request();
    assert_eq!(request["method"], "turn/start");
    assert_eq!(request["params"]["threadId"], "thread-main");
    let text = request["params"]["input"][0]["text"].as_str().unwrap();
    assert!(!text.contains(&mail.message.message_id));
    assert!(!text.contains("Which interface"));
    let context = request["params"]["additionalContext"].as_object().unwrap();
    assert_eq!(context.len(), 1);
    let value = context.values().next().unwrap();
    assert_eq!(value["kind"], "untrusted");
    assert_eq!(value["name"], "rudder_peer");
    let peer: PeerMessage = serde_json::from_str(value["value"].as_str().unwrap()).unwrap();
    assert_eq!(peer.message_id, mail.message.message_id);
    assert_eq!(peer.sender_run_id, "peer");
    assert!(peer.text.contains("principal-peer"));
    assert!(peer.text.contains("agents_inbox"));
    assert!(!peer.text.contains("Which interface"));
    server.frame(started("mail-turn"));
    server.frame(
        json!({"id":request["id"],"result":{"turn":{"id":"mail-turn","status":"inProgress"}}}),
    );
    let receipt = until(
        &events,
        |event| matches!(event,AgentLinkEventKind::CommandResult {command_id,outcome:AgentLinkOutcome::TransportAccepted {turn_id}} if command_id==&mail.claim.command_id && turn_id=="mail-turn"),
    );
    let AgentLinkEventKind::CommandResult { outcome, .. } = receipt.kind else {
        unreachable!()
    };
    mail.journal
        .finish_effect(&mail.claim, &json!({"adapter":outcome}), NOW + 3)
        .unwrap();
    assert!(mail
        .journal
        .claim_idle_mailbox_delivery(&mail.owner, "run-one", &server.launch.attempt_id, NOW + 4)
        .unwrap()
        .is_none());
    mail.still_unacknowledged();
    link.close();
    link.await_closed(Duration::from_secs(3)).unwrap();
    assert!(!server
        .all_requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| matches!(
            request["method"].as_str(),
            Some("turn/interrupt" | "turn/steer" | "thread/start" | "thread/fork")
        )));
}

#[test]
fn automatic_mail_actual_websocket_lost_reply_retains_uncertain_original_without_replay() {
    let server = MockServer::new(None, false);
    let (mut link, events) = server.spawn();
    until(&events, |event| {
        matches!(event, AgentLinkEventKind::Bound { .. })
    });
    let mut mail = Mail::new(&server);
    mail.submit(&link);
    let request = server.next_request();
    assert_eq!(request["method"], "turn/start");
    server.actions.send(ServerAction::Disconnect).unwrap();
    until(
        &events,
        |event| matches!(event,AgentLinkEventKind::CommandResult {command_id,outcome:AgentLinkOutcome::Uncertain {..}} if command_id==&mail.claim.command_id),
    );
    mail.journal.mark_effect_uncertain(&mail.claim,&json!({"fixture":"actual original RPC bytes received; connection closed before reply"}),NOW+3).unwrap();
    assert_eq!(
        mail.journal
            .command_receipt(&mail.owner, &mail.claim.command_id)
            .unwrap()
            .state,
        CommandState::Uncertain
    );
    assert!(mail
        .journal
        .claim_idle_mailbox_delivery(&mail.owner, "run-one", &server.launch.attempt_id, NOW + 4)
        .unwrap()
        .is_none());
    mail.still_unacknowledged();
    link.close();
    link.await_closed(Duration::from_secs(3)).unwrap();
    assert_eq!(
        server
            .all_requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .count(),
        1
    );
}
