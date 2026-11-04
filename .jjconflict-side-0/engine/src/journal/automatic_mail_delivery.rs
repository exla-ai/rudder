//! The existing owner may notify a qualified idle session about its durable
//! inbox. This uses the command/effect ledger, never a second scheduler. A
//! notification write does not acknowledge mail or grant control authority.
use super::*;

const PREFIX: &str = "mailbox-auto:";
const DELIVERY_TIMEOUT_MS: i64 = 30_000;

/// Bounded DATA naming an existing authenticated envelope. This contains no
/// duplicated peer-controlled body and cannot assert execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailboxDeliveryNotice {
    pub message_id: String,
    pub project_id: String,
    pub author: String,
    pub sender_run: String,
    pub sender_epoch: u64,
    pub recipient_run: String,
    pub recipient_epoch: u64,
    pub intent: MessageIntent,
}
impl MailboxDeliveryNotice {
    fn from_message(message: &MessageEnvelope) -> Self {
        Self {
            message_id: message.message_id.clone(),
            project_id: message.project_id.clone(),
            author: message.author.clone(),
            sender_run: message.sender_run.clone(),
            sender_epoch: message.sender_epoch,
            recipient_run: message.request.recipient_run.clone(),
            recipient_epoch: message.request.recipient_epoch,
            intent: message.request.intent,
        }
    }
}

/// An original started effect, committed before the caller can write to a
/// backend. No Deserialize/Clone constructor can assert this transaction ran.
pub struct ClaimedMailboxDelivery {
    claim: EffectClaim,
    accepted_at_ms: i64,
}
impl ClaimedMailboxDelivery {
    pub fn into_effect(self) -> (EffectClaim, i64) {
        (self.claim, self.accepted_at_ms)
    }
}

pub(super) fn reserve_agent_key(key: &str) -> Result<()> {
    ensure!(
        !key.starts_with(PREFIX),
        "mailbox delivery keys belong to the engine"
    );
    Ok(())
}

fn idle(snapshot: &RuntimeSnapshot) -> bool {
    let value = &snapshot.observation;
    !snapshot.stale
        && value.pty_owned
        && matches!(value.phase, RuntimePhase::Waiting | RuntimePhase::Completed)
        && value.active_turn_id.is_none()
        && value.last_interrupted_turn_id.is_none()
        && value.thread_id.is_some()
        && value.capabilities.contains("deliver")
        && matches!(
            value.execution_profile.as_str(),
            "codex-app-server" | "claude-sdk"
        )
}

fn payload(message: &MessageEnvelope, snapshot: &RuntimeSnapshot, timeout: i64) -> Value {
    serde_json::json!({
        "automatic_mailbox_delivery": 1,
        "mailbox_notice": MailboxDeliveryNotice::from_message(message),
        "expected_thread_id": snapshot.observation.thread_id,
        "expected_turn_id": null,
        "expected_attempt_id": snapshot.observation.attempt_id,
        "timeout_ms": timeout,
    })
}

impl Journal {
    /// Called only by the actual engine actor after its in-memory PTY/status
    /// check. The same write transaction independently checks current owner,
    /// attempt, idle backend observation, addressed outbox, and effect ordering.
    /// At most one notification is ever started for a message. Uncertainty or
    /// a crash retains that original effect; a later tick cannot resend it.
    pub fn claim_idle_mailbox_delivery(
        &mut self,
        owner: &OwnerFence,
        run: &str,
        attempt: &str,
        now_ms: i64,
    ) -> Result<Option<ClaimedMailboxDelivery>> {
        valid_id(run)?;
        valid_id(attempt)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let Some(snapshot) =
            super::runtime::runtime_snapshot(&tx, &owner.project_id, run, owner.epoch, now_ms)?
        else {
            return Ok(None);
        };
        if !idle(&snapshot) || snapshot.observation.attempt_id != attempt {
            return Ok(None);
        }
        let epoch = snapshot.observation.run_epoch;
        assert_run(&tx, &owner.project_id, run, epoch)?;
        // Never jump over an explicit control or an uncertain prior write.
        let blocked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM commands WHERE project_id=?1 AND target_run=?2 AND target_epoch=?3 AND state!='completed')",
            params![owner.project_id, run, sql_u64(epoch)?], |r| r.get(0),
        )?;
        if blocked {
            return Ok(None);
        }
        let encoded: Option<String> = tx.query_row(
            "SELECT m.envelope_json FROM deliveries d JOIN messages m ON m.message_id=d.message_id WHERE d.project_id=?1 AND d.recipient_run=?2 AND d.recipient_epoch=?3 AND d.status='pending' AND m.expires_at>?4 AND NOT EXISTS(SELECT 1 FROM commands c WHERE c.project_id=m.project_id AND c.principal=m.principal AND c.idempotency_key=('mailbox-auto:' || m.message_id)) ORDER BY CASE json_extract(m.envelope_json,'$.request.priority') WHEN 'urgent' THEN 0 ELSE 1 END,d.sequence LIMIT 1",
            params![owner.project_id, run, sql_u64(epoch)?, now_ms], |r| r.get(0),
        ).optional()?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let message: MessageEnvelope = serde_json::from_str(&encoded)?;
        ensure!(
            message.project_id == owner.project_id
                && message.request.recipient_run == run
                && message.request.recipient_epoch == epoch,
            "addressed message identity differs"
        );
        let timeout = message
            .request
            .expires_at_ms
            .checked_sub(now_ms)
            .context("message expiry overflow")?
            .min(DELIVERY_TIMEOUT_MS);
        ensure!(timeout > 0, "message already expired");
        let request = CommandRequest {
            idempotency_key: format!("{PREFIX}{}", message.message_id),
            target_run: run.into(),
            target_epoch: epoch,
            kind: "deliver".into(),
            scope: message.request.scope.clone(),
            payload: payload(&message, &snapshot, timeout),
        };
        let command_id = Uuid::new_v4().to_string();
        let claim_id = Uuid::new_v4().to_string();
        let sequence = append_event(
            &tx,
            &owner.project_id,
            "mailbox-notification-accepted",
            &message.author,
            &serde_json::json!({"command_id":command_id,"message_id":message.message_id,"recipient_attempt":attempt}),
            now_ms,
        )?;
        tx.execute(
            "INSERT INTO commands(command_id,project_id,principal,idempotency_key,request_json,target_run,target_epoch,kind,payload_json,accepted_sequence,accepted_at,sender_run,sender_epoch,state,capability_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,'deliver',?8,?9,?10,?11,?12,'started',NULL)",
            params![command_id,owner.project_id,message.author,request.idempotency_key,json(&request)?,run,sql_u64(epoch)?,json(&request.payload)?,sql_u64(sequence)?,now_ms,message.sender_run,sql_u64(message.sender_epoch)?],
        )?;
        tx.execute("INSERT INTO effects(command_id,state,claim_id,owner_epoch,started_at) VALUES(?1,'started',?2,?3,?4)",
            params![command_id,claim_id,sql_u64(owner.epoch)?,now_ms])?;
        append_event(
            &tx,
            &owner.project_id,
            "effect-started",
            &owner.owner_id,
            &serde_json::json!({"command_id":command_id,"claim_id":claim_id,"message_id":message.message_id,"target_epoch":epoch,"owner_epoch":owner.epoch}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(Some(ClaimedMailboxDelivery {
            claim: EffectClaim {
                command_id,
                claim_id,
                owner: owner.clone(),
                principal: message.author,
                sender_run: message.sender_run,
                sender_epoch: message.sender_epoch,
                target_run: run.into(),
                target_epoch: epoch,
                kind: "deliver".into(),
                payload: request.payload,
            },
            accepted_at_ms: now_ms,
        }))
    }
}

/// Only explicit owner reconciliation can ever put an automatic effect back
/// into pending. Revalidate its original DATA and immutable clock, never bind
/// that original notification to a new session, attempt or publication window.
pub(super) fn validate_reconciled_claim(
    conn: &Connection,
    command: &str,
    now_ms: i64,
) -> Result<bool> {
    let (key, encoded, principal, sender, sender_epoch, accepted): (String,String,String,String,u64,i64) = conn.query_row(
        "SELECT idempotency_key,request_json,principal,sender_run,sender_epoch,accepted_at FROM commands WHERE command_id=?1",
        [command], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,db_u64(r,4)?,r.get(5)?)),
    )?;
    if !key.starts_with(PREFIX) {
        return Ok(false);
    }
    let request: CommandRequest = serde_json::from_str(&encoded)?;
    let notice: MailboxDeliveryNotice =
        serde_json::from_value(request.payload["mailbox_notice"].clone())?;
    let (original,status): (String,String) = conn.query_row(
        "SELECT m.envelope_json,d.status FROM messages m JOIN deliveries d ON d.message_id=m.message_id WHERE m.message_id=?1 AND m.project_id=?2",
        params![notice.message_id,notice.project_id], |r| Ok((r.get(0)?,r.get(1)?)),
    )?;
    let message: MessageEnvelope = serde_json::from_str(&original)?;
    ensure!(
        MailboxDeliveryNotice::from_message(&message) == notice && status == "pending",
        "original addressed mail was resolved or changed"
    );
    ensure!(
        key == format!("{PREFIX}{}", message.message_id)
            && request.kind == "deliver"
            && message.author == principal
            && message.sender_run == sender
            && message.sender_epoch == sender_epoch
            && request.target_run == message.request.recipient_run
            && request.target_epoch == message.request.recipient_epoch,
        "original mailbox notification identity changed"
    );
    let owner_epoch: u64 = conn.query_row(
        "SELECT owner_epoch FROM projects WHERE project_id=?1",
        [&message.project_id],
        |r| db_u64(r, 0),
    )?;
    let snapshot = super::runtime::runtime_snapshot(
        conn,
        &message.project_id,
        &request.target_run,
        owner_epoch,
        now_ms,
    )?
    .context("original notification runtime absent")?;
    let timeout = message
        .request
        .expires_at_ms
        .checked_sub(accepted)
        .context("notification expiry overflow")?
        .min(DELIVERY_TIMEOUT_MS);
    ensure!(
        now_ms >= accepted
            && now_ms
                .checked_sub(accepted)
                .is_some_and(|age| age < timeout),
        "original mailbox notification expired"
    );
    ensure!(
        idle(&snapshot) && request.payload == payload(&message, &snapshot, timeout),
        "original notification session or attempt changed"
    );
    Ok(true)
}

#[cfg(test)]
#[path = "automatic_mail_delivery_tests.rs"]
mod tests;
