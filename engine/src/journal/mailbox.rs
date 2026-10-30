use super::*;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_INBOX_COUNT: i64 = 256;
const MAX_INBOX_BYTES: i64 = 4 * 1024 * 1024;
const MAX_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageIntent {
    Inform,
    Question,
    Answer,
    Propose,
    InterfaceChange,
    Checkpoint,
    Complete,
    RequestIntegration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePriority {
    #[default]
    Normal,
    Urgent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageRequest {
    pub idempotency_key: String,
    pub recipient_run: String,
    pub recipient_epoch: u64,
    pub scope: String,
    pub intent: MessageIntent,
    pub body: Value,
    pub expires_at_ms: i64,
    #[serde(default)]
    pub priority: MessagePriority,
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Identifies an original question. Replies correlate through reply_to.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Stable message ID of the original request, not arbitrary conversation text.
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub topic_versions: TopicVersions,
    #[serde(default)]
    pub artifacts: Vec<ObjectRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEnvelope {
    pub message_id: String,
    pub project_id: String,
    pub sequence: u64,
    /// Derived from the issued capability, never asserted in the message body.
    pub author: String,
    pub sender_run: String,
    pub sender_epoch: u64,
    pub created_at_ms: i64,
    pub correlation_id: Option<String>,
    pub request: MessageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxCursor {
    pub project_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    /// Highest contiguous resolved entry in this recipient's addressed stream.
    /// Other recipients' project-sequence gaps do not block this cursor.
    pub sequence: u64,
}

impl Journal {
    /// Scoped durable sender lookup supports stable automatic TTLs on API
    /// retries without generating a different message body after reconnect.
    pub fn sent_message(
        &self,
        token: &str,
        key: &str,
        now_ms: i64,
    ) -> Result<Option<MessageEnvelope>> {
        let tx = self.conn.unchecked_transaction()?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "message:send")?;
        let original:Option<String>=tx.query_row("SELECT envelope_json FROM messages WHERE project_id=?1 AND principal=?2 AND idempotency_key=?3",params![auth.project,auth.grant.principal,key],|row|row.get(0)).optional()?;
        let result = if let Some(original) = original {
            let envelope: MessageEnvelope = serde_json::from_str(&original)?;
            ensure!(
                envelope.sender_run == auth.grant.run_id
                    && envelope.sender_epoch == auth.grant.run_epoch,
                "message belongs to another sender attempt"
            );
            Some(envelope)
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }

    pub fn received_message(
        &self,
        token: &str,
        message_id: &str,
        now_ms: i64,
    ) -> Result<MessageEnvelope> {
        let tx = self.conn.unchecked_transaction()?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "mailbox:read")?;
        let envelope = message(&tx, message_id)?;
        ensure!(
            envelope.project_id == auth.project
                && envelope.request.recipient_run == auth.grant.run_id
                && envelope.request.recipient_epoch == auth.grant.run_epoch,
            "message is outside recipient attempt"
        );
        tx.commit()?;
        Ok(envelope)
    }

    pub fn send_message(
        &mut self,
        token: &str,
        request: &MessageRequest,
        now_ms: i64,
    ) -> Result<MessageEnvelope> {
        self.send_message_internal(token, request, None, now_ms)
    }

    /// Automatic expiry is derived under the same write transaction as
    /// deduplication. Concurrent first sends cannot choose conflicting times.
    pub fn send_message_with_ttl(
        &mut self,
        token: &str,
        request: &MessageRequest,
        ttl_ms: u64,
        now_ms: i64,
    ) -> Result<MessageEnvelope> {
        ensure!(
            (1..=MAX_TTL_MS as u64).contains(&ttl_ms),
            "invalid message TTL"
        );
        self.send_message_internal(token, request, Some(i64::try_from(ttl_ms)?), now_ms)
    }

    fn send_message_internal(
        &mut self,
        token: &str,
        request: &MessageRequest,
        ttl_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<MessageEnvelope> {
        let mut adjusted = request.clone();
        if let Some(ttl) = ttl_ms {
            adjusted.expires_at_ms = now_ms.checked_add(ttl).context("message expiry overflow")?;
        }
        let request = &mut adjusted;
        valid_id(&request.idempotency_key)?;
        for id in [
            &request.conversation_id,
            &request.request_id,
            &request.reply_to,
            &request.supersedes,
        ]
        .into_iter()
        .flatten()
        {
            valid_id(id)?;
        }
        ensure!(
            request.intent != MessageIntent::Answer || request.reply_to.is_some(),
            "an answer must reference a request"
        );
        for reference in &request.artifacts {
            reference.validate()?;
        }
        let mut body = json(request)?;
        ensure!(
            body.len() <= MAX_MESSAGE_BYTES,
            "message exceeds payload limit"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "message:send")?;
        ensure!(
            request
                .topic_versions
                .keys()
                .all(|topic| auth.grant.topics.contains(topic)),
            "message topic is outside capability"
        );
        let duplicate:Option<(String,String)>=tx.query_row("SELECT envelope_json,request_json FROM messages WHERE project_id=?1 AND principal=?2 AND idempotency_key=?3",params![auth.project,auth.grant.principal,request.idempotency_key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((original, original_body)) = duplicate {
            let envelope: MessageEnvelope = serde_json::from_str(&original)?;
            if let Some(ttl) = ttl_ms {
                request.expires_at_ms = envelope
                    .created_at_ms
                    .checked_add(ttl)
                    .context("message expiry overflow")?;
                body = json(request)?;
            }
            ensure!(
                original_body == body,
                "idempotency key reused with a different message"
            );
            ensure!(
                envelope.sender_run == auth.grant.run_id
                    && envelope.sender_epoch == auth.grant.run_epoch,
                "idempotency key belongs to another sender attempt"
            );
            return Ok(envelope);
        }
        // A deduplicated send is a historical sender receipt. It never routes
        // into a replacement attempt; new messages require current authority.
        authorize_target(
            &tx,
            &auth,
            &request.recipient_run,
            request.recipient_epoch,
            &request.scope,
        )?;
        ensure!(
            request.expires_at_ms > now_ms
                && request.expires_at_ms
                    <= now_ms.checked_add(MAX_TTL_MS).context("invalid expiry")?,
            "message expiry must be within seven days"
        );
        let correlation_id = if let Some(reply) = &request.reply_to {
            let original = message(&tx, reply)?;
            ensure!(
                original.project_id == auth.project
                    && original.request.recipient_run == auth.grant.run_id
                    && original.request.recipient_epoch == auth.grant.run_epoch,
                "reply request is outside this recipient attempt"
            );
            ensure!(
                original.sender_run == request.recipient_run
                    && original.sender_epoch == request.recipient_epoch,
                "reply must address the original sender attempt"
            );
            ensure!(
                original.request.request_id.is_some() && original.request.expires_at_ms > now_ms,
                "request is absent or expired"
            );
            ensure!(
                original.request.conversation_id == request.conversation_id,
                "reply conversation mismatch"
            );
            original.request.request_id
        } else {
            request.request_id.clone()
        };
        expire(
            &tx,
            &auth.project,
            &request.recipient_run,
            request.recipient_epoch,
            now_ms,
        )?;
        if let Some(previous_id) = &request.supersedes {
            let previous = message(&tx, previous_id)?;
            ensure!(
                informational(request.intent) && informational(previous.request.intent),
                "conversations and decisions cannot be coalesced"
            );
            ensure!(
                previous.project_id == auth.project
                    && previous.author == auth.grant.principal
                    && previous.sender_run == auth.grant.run_id
                    && previous.sender_epoch == auth.grant.run_epoch
                    && previous.request.recipient_run == request.recipient_run
                    && previous.request.recipient_epoch == request.recipient_epoch
                    && previous.request.scope == request.scope,
                "supersession is outside original sender/recipient scope"
            );
            tx.execute("UPDATE deliveries SET status='skipped',ack_kind='superseded',updated_at=?1 WHERE message_id=?2 AND status='pending'",params![now_ms,previous_id])?;
        }
        let(count,bytes):(i64,i64)=tx.query_row("SELECT count(*),COALESCE(sum(m.byte_size),0) FROM deliveries d JOIN messages m ON m.message_id=d.message_id WHERE d.project_id=?1 AND d.recipient_run=?2 AND d.recipient_epoch=?3 AND d.status='pending'",params![auth.project,request.recipient_run,sql_u64(request.recipient_epoch)?],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            count < MAX_INBOX_COUNT && bytes + body.len() as i64 <= MAX_INBOX_BYTES,
            "recipient mailbox is full"
        );
        let id = Uuid::new_v4().to_string();
        let sequence = append_event(
            &tx,
            &auth.project,
            "message-recorded",
            &auth.grant.principal,
            &serde_json::json!({"message_id":id,"sender_run":auth.grant.run_id,"sender_epoch":auth.grant.run_epoch,"request":request}),
            now_ms,
        )?;
        let envelope = MessageEnvelope {
            message_id: id.clone(),
            project_id: auth.project.clone(),
            sequence,
            author: auth.grant.principal.clone(),
            sender_run: auth.grant.run_id.clone(),
            sender_epoch: auth.grant.run_epoch,
            created_at_ms: now_ms,
            correlation_id,
            request: request.clone(),
        };
        tx.execute("INSERT INTO messages(message_id,project_id,principal,idempotency_key,request_json,envelope_json,sender_run,sender_epoch,recipient_run,recipient_epoch,sequence,expires_at,byte_size,request_id,reply_to) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![id,auth.project,auth.grant.principal,request.idempotency_key,body,json(&envelope)?,auth.grant.run_id,sql_u64(auth.grant.run_epoch)?,request.recipient_run,sql_u64(request.recipient_epoch)?,sql_u64(sequence)?,request.expires_at_ms,body.len() as i64,request.request_id,request.reply_to])?;
        // This pending delivery is the durable addressed outbox. It is retained
        // across reads and restarts until this receiving attempt commits an ack
        // or an explicit expiry/skip/supersession resolves it.
        tx.execute("INSERT INTO deliveries(message_id,project_id,recipient_run,recipient_epoch,sequence,status,updated_at) VALUES(?1,?2,?3,?4,?5,'pending',?6)",params![id,auth.project,request.recipient_run,sql_u64(request.recipient_epoch)?,sql_u64(sequence)?,now_ms])?;
        advance_cursor(
            &tx,
            &auth.project,
            &request.recipient_run,
            request.recipient_epoch,
        )?;
        tx.commit()?;
        Ok(envelope)
    }

    pub fn inbox(
        &mut self,
        token: &str,
        limit: usize,
        now_ms: i64,
    ) -> Result<Vec<MessageEnvelope>> {
        ensure!((1..=256).contains(&limit), "inbox page must be 1..256");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "mailbox:read")?;
        expire(
            &tx,
            &auth.project,
            &auth.grant.run_id,
            auth.grant.run_epoch,
            now_ms,
        )?;
        let envelopes = {
            let mut stmt=tx.prepare("SELECT m.envelope_json FROM deliveries d JOIN messages m ON m.message_id=d.message_id WHERE d.project_id=?1 AND d.recipient_run=?2 AND d.recipient_epoch=?3 AND d.status='pending' ORDER BY CASE json_extract(m.envelope_json,'$.request.priority') WHEN 'urgent' THEN 0 ELSE 1 END,d.sequence LIMIT ?4")?;
            let rows = stmt
                .query_map(
                    params![
                        auth.project,
                        auth.grant.run_id,
                        sql_u64(auth.grant.run_epoch)?,
                        limit as i64
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .map(|s| serde_json::from_str(&s).map_err(Into::into))
                .collect::<Result<Vec<MessageEnvelope>>>()?
        };
        tx.commit()?;
        Ok(envelopes)
    }

    /// Commits receipt by the scoped receiving engine/adapter. This does not
    /// assert that the model adopted the message or that any code complies.
    pub fn ack_message(
        &mut self,
        token: &str,
        message_id: &str,
        now_ms: i64,
    ) -> Result<MailboxCursor> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "mailbox:ack")?;
        let envelope = message(&tx, message_id)?;
        ensure!(
            envelope.project_id == auth.project
                && envelope.request.recipient_run == auth.grant.run_id
                && envelope.request.recipient_epoch == auth.grant.run_epoch,
            "message is outside recipient attempt"
        );
        expire(
            &tx,
            &auth.project,
            &auth.grant.run_id,
            auth.grant.run_epoch,
            now_ms,
        )?;
        let status: String = tx.query_row(
            "SELECT status FROM deliveries WHERE message_id=?1",
            [message_id],
            |r| r.get(0),
        )?;
        ensure!(
            status == "pending" || status == "acked",
            "message was explicitly skipped or expired"
        );
        if status == "pending" {
            tx.execute("UPDATE deliveries SET status='acked',ack_kind='received',updated_at=?1 WHERE message_id=?2",params![now_ms,message_id])?;
            append_event(
                &tx,
                &auth.project,
                "message-receipt-committed",
                &auth.grant.principal,
                &serde_json::json!({"message_id":message_id,"recipient_run":auth.grant.run_id,"recipient_epoch":auth.grant.run_epoch}),
                now_ms,
            )?;
        }
        let cursor = advance_cursor(&tx, &auth.project, &auth.grant.run_id, auth.grant.run_epoch)?;
        tx.commit()?;
        Ok(cursor)
    }

    pub fn mailbox_cursor(&self, token: &str, now_ms: i64) -> Result<MailboxCursor> {
        let auth = authenticate(&self.conn, token, now_ms)?;
        authorize(&self.conn, &auth, "mailbox:read")?;
        cursor(
            &self.conn,
            &auth.project,
            &auth.grant.run_id,
            auth.grant.run_epoch,
        )
    }

    /// Explicit trusted-engine cleanup, including old-epoch handoff records.
    /// There is deliberately no deletion/GC API that could erase an ack hole.
    pub fn skip_message(
        &mut self,
        owner: &OwnerFence,
        message_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<MailboxCursor> {
        valid_id(reason)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let envelope = message(&tx, message_id)?;
        ensure!(
            envelope.project_id == owner.project_id,
            "message is outside project"
        );
        let changed=tx.execute("UPDATE deliveries SET status='skipped',ack_kind=?1,updated_at=?2 WHERE message_id=?3 AND status='pending'",params![reason,now_ms,message_id])?;
        if changed > 0 {
            append_event(
                &tx,
                &owner.project_id,
                "message-skipped",
                &owner.owner_id,
                &serde_json::json!({"message_id":message_id,"reason":reason}),
                now_ms,
            )?;
        }
        let resolved = advance_cursor(
            &tx,
            &owner.project_id,
            &envelope.request.recipient_run,
            envelope.request.recipient_epoch,
        )?;
        tx.commit()?;
        Ok(resolved)
    }
}

fn informational(intent: MessageIntent) -> bool {
    matches!(intent, MessageIntent::Inform | MessageIntent::Checkpoint)
}
fn message(conn: &Connection, id: &str) -> Result<MessageEnvelope> {
    let body: String = conn
        .query_row(
            "SELECT envelope_json FROM messages WHERE message_id=?1",
            [id],
            |r| r.get(0),
        )
        .context("unknown message")?;
    Ok(serde_json::from_str(&body)?)
}
fn cursor(conn: &Connection, project: &str, run: &str, epoch: u64) -> Result<MailboxCursor> {
    let sequence = conn
        .query_row(
            "SELECT cursor FROM mailbox_cursors WHERE project_id=?1 AND run_id=?2 AND run_epoch=?3",
            params![project, run, sql_u64(epoch)?],
            |r| db_u64(r, 0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(MailboxCursor {
        project_id: project.into(),
        run_id: run.into(),
        run_epoch: epoch,
        sequence,
    })
}
fn advance_cursor(
    tx: &Transaction<'_>,
    project: &str,
    run: &str,
    epoch: u64,
) -> Result<MailboxCursor> {
    let old = cursor(tx, project, run, epoch)?;
    let next:u64=tx.query_row("SELECT COALESCE(max(sequence),?4) FROM deliveries WHERE project_id=?1 AND recipient_run=?2 AND recipient_epoch=?3 AND status!='pending' AND sequence<COALESCE((SELECT min(sequence) FROM deliveries WHERE project_id=?1 AND recipient_run=?2 AND recipient_epoch=?3 AND status='pending'),9223372036854775807)",params![project,run,sql_u64(epoch)?,sql_u64(old.sequence)?],|r|db_u64(r,0))?;
    ensure!(next >= old.sequence, "mailbox cursor would regress");
    tx.execute("INSERT INTO mailbox_cursors(project_id,run_id,run_epoch,cursor) VALUES(?1,?2,?3,?4) ON CONFLICT(project_id,run_id,run_epoch) DO UPDATE SET cursor=excluded.cursor WHERE excluded.cursor>mailbox_cursors.cursor",params![project,run,sql_u64(epoch)?,sql_u64(next)?])?;
    Ok(MailboxCursor {
        sequence: next,
        ..old
    })
}
fn expire(tx: &Transaction<'_>, project: &str, run: &str, epoch: u64, now_ms: i64) -> Result<()> {
    let count=tx.execute("UPDATE deliveries SET status='expired',ack_kind='expired',updated_at=?1 WHERE project_id=?2 AND recipient_run=?3 AND recipient_epoch=?4 AND status='pending' AND message_id IN(SELECT message_id FROM messages WHERE expires_at<=?1)",params![now_ms,project,run,sql_u64(epoch)?])?;
    if count > 0 {
        append_event(
            tx,
            project,
            "messages-expired",
            "engine",
            &serde_json::json!({"recipient_run":run,"recipient_epoch":epoch,"count":count}),
            now_ms,
        )?;
    }
    advance_cursor(tx, project, run, epoch)?;
    Ok(())
}
