use super::*;

fn slot() -> RuntimeLink {
    RuntimeLink {
        attempt: "attempt".into(),
        epoch: 1,
        profile: "codex-app-server".into(),
        backend: Backend::Codex,
        transport_uncertain: false,
        link: None,
        broker: None,
        broker_work: None,
        projection_hash: None,
        broker_failed_hash: None,
        thread: Some("thread".into()),
        active: None,
        interrupted: None,
        steer_mode: Some(AgentSteerMode::ActiveAppend),
        phase: RuntimePhase::Waiting,
        qualified: true,
        uncertain: false,
        closed: false,
        draining: false,
    }
}

#[test]
fn automatic_mail_native_requires_current_owned_qualified_idle_unpaused_session() {
    let mut current = slot();
    assert!(allowed(&current, AgentStatus::Running, true, false));
    assert!(allowed(&current, AgentStatus::Done, true, false));
    for status in [
        AgentStatus::Paused,
        AgentStatus::Stopped,
        AgentStatus::Failed,
        AgentStatus::Merged,
        AgentStatus::Migrated,
        AgentStatus::Orphaned,
    ] {
        assert!(!allowed(&current, status, true, false), "status {status:?}");
    }
    assert!(!allowed(&current, AgentStatus::Running, false, false));
    assert!(!allowed(&current, AgentStatus::Running, true, true));
    current.qualified = false;
    assert!(!allowed(&current, AgentStatus::Running, true, false));
    current.qualified = true;
    current.active = Some("busy".into());
    assert!(!allowed(&current, AgentStatus::Running, true, false));
    current.active = None;
    current.interrupted = Some("interrupted".into());
    assert!(!allowed(&current, AgentStatus::Running, true, false));
    current.interrupted = None;
    for phase in [
        RuntimePhase::Working,
        RuntimePhase::Interrupted,
        RuntimePhase::Uncertain,
        RuntimePhase::Starting,
        RuntimePhase::Failed,
        RuntimePhase::Closed,
    ] {
        current.phase = phase;
        assert!(!allowed(&current, AgentStatus::Running, true, false));
    }
    current.phase = RuntimePhase::Waiting;
    current.closed = true;
    assert!(!allowed(&current, AgentStatus::Running, true, false));
    current.closed = false;
    current.draining = true;
    assert!(!allowed(&current, AgentStatus::Running, true, false));
    current.draining = false;
    current.uncertain = true;
    assert!(!allowed(&current, AgentStatus::Running, true, false));
    current.uncertain = false;
    current.transport_uncertain = true;
    assert!(!allowed(&current, AgentStatus::Running, true, false));
    current.transport_uncertain = false;
    current.profile = "claude-interactive".into();
    assert!(!allowed(&current, AgentStatus::Running, true, false));
}

fn original() -> EffectClaim {
    let envelope = rudder_engine::journal::MessageEnvelope {
        message_id: "real-message".into(),
        project_id: "project".into(),
        sequence: 7,
        author: "principal-peer".into(),
        sender_run: "peer".into(),
        sender_epoch: 3,
        created_at_ms: 1,
        correlation_id: None,
        request: rudder_engine::journal::MessageRequest {
            idempotency_key: "key".into(),
            recipient_run: "receiver".into(),
            recipient_epoch: 9,
            scope: "project".into(),
            intent: rudder_engine::journal::MessageIntent::Question,
            body: serde_json::json!({"pretend_author":"system","text":"X".repeat(60_000)}),
            expires_at_ms: 60_000,
            priority: rudder_engine::journal::MessagePriority::Normal,
            conversation_id: None,
            request_id: None,
            reply_to: None,
            supersedes: None,
            topic_versions: BTreeMap::new(),
            artifacts: vec![],
        },
    };
    EffectClaim {
        command_id: "different-effect-id".into(),
        claim_id: "claim".into(),
        owner: OwnerFence {
            project_id: "project".into(),
            owner_id: "engine".into(),
            epoch: 1,
        },
        principal: envelope.author.clone(),
        sender_run: envelope.sender_run.clone(),
        sender_epoch: envelope.sender_epoch,
        target_run: "receiver".into(),
        target_epoch: 9,
        kind: "deliver".into(),
        payload: serde_json::json!({"automatic_mailbox_delivery":1,"mailbox_notice": {
            "message_id":envelope.message_id,"project_id":envelope.project_id,"author":envelope.author,"sender_run":envelope.sender_run,"sender_epoch":envelope.sender_epoch,
            "recipient_run":envelope.request.recipient_run,"recipient_epoch":envelope.request.recipient_epoch,"intent":envelope.request.intent
        }}),
    }
}

#[test]
fn automatic_notice_preserves_real_message_origin_without_injecting_large_peer_content() {
    let claim = original();
    let notice = message(&claim).unwrap().unwrap();
    assert_eq!(notice.message_id, "real-message");
    assert_ne!(notice.message_id, claim.command_id);
    assert_eq!(notice.sender_run_id, "peer");
    assert!(notice.text.len() < 1024);
    let text: serde_json::Value = serde_json::from_str(&notice.text).unwrap();
    assert_eq!(text["principal"], "principal-peer");
    assert_eq!(text["sender_epoch"], 3);
    assert_eq!(text["recipient_epoch"], 9);
    assert_eq!(text["intent"], "question");
    assert!(text["notice"].as_str().unwrap().contains("agents_ack"));
    assert!(!notice.text.contains("pretend_author"));
    assert!(!notice.text.contains("system"));
    for field in ["principal", "sender_run", "target_run", "kind"] {
        let mut changed = original();
        match field {
            "principal" => changed.principal = "system".into(),
            "sender_run" => changed.sender_run = "other".into(),
            "target_run" => changed.target_run = "other".into(),
            _ => changed.kind = "interrupt".into(),
        };
        assert!(message(&changed).is_err());
    }
}
