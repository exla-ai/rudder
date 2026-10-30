use super::*;

const MAX_COMMAND_BYTES: usize = 256 * 1024;
const MAX_PENDING_COMMANDS: i64 = 256;
const MAX_PENDING_BYTES: i64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub idempotency_key: String,
    pub target_run: String,
    pub target_epoch: u64,
    pub kind: String,
    pub scope: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Accepted,
    Started,
    Uncertain,
    Completed,
}

impl CommandState {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "accepted" => Self::Accepted,
            "started" => Self::Started,
            "uncertain" => Self::Uncertain,
            "completed" => Self::Completed,
            _ => bail!("invalid command state"),
        })
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Started => "started",
            Self::Uncertain => "uncertain",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: String,
    pub project_id: String,
    pub accepted_sequence: u64,
    pub accepted_at_ms: i64,
    pub state: CommandState,
    pub result: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectClaim {
    pub command_id: String,
    pub claim_id: String,
    pub owner: OwnerFence,
    /// Authenticated at acceptance and copied from the durable command row.
    /// Peer-controlled payload text cannot supply or override this origin.
    pub principal: String,
    pub sender_run: String,
    pub sender_epoch: u64,
    pub target_run: String,
    pub target_epoch: u64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recovery {
    /// Pages are bounded; use `commands_in_state` to inspect additional rows.
    pub pending: Vec<CommandReceipt>,
    pub uncertain: Vec<CommandReceipt>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Reconciliation {
    Completed {
        result: Value,
        evidence: Value,
    },
    /// Use only after proving that the effect did not occur, or that the
    /// external service safely deduplicates this exact command identifier.
    RetrySafe {
        evidence: Value,
    },
}

impl Journal {
    pub fn accept_command(
        &mut self,
        token: &str,
        request: &CommandRequest,
        now_ms: i64,
    ) -> Result<CommandReceipt> {
        valid_id(&request.idempotency_key)?;
        valid_id(&request.kind)?;
        let body = json(request)?;
        ensure!(
            body.len() <= MAX_COMMAND_BYTES,
            "command exceeds payload limit"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, &format!("command:{}", request.kind))?;
        let duplicate:Option<(String,String,String,u64)>=tx.query_row("SELECT command_id,request_json,sender_run,sender_epoch FROM commands WHERE project_id=?1 AND principal=?2 AND idempotency_key=?3",params![auth.project,auth.grant.principal,request.idempotency_key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,db_u64(r,3)?))).optional()?;
        if let Some((id, original, sender, epoch)) = duplicate {
            ensure!(
                sender == auth.grant.run_id && epoch == auth.grant.run_epoch,
                "idempotency key belongs to another sender attempt"
            );
            ensure!(
                original == body,
                "idempotency key reused with a different command"
            );
            return receipt(&tx, &id);
        }
        // Retries observe their original durable outcome even after a target
        // restarts. Only a NEW effect needs authority over the current target.
        authorize_target(
            &tx,
            &auth,
            &request.target_run,
            request.target_epoch,
            &request.scope,
        )?;
        super::runtime::validate_live_control(&tx, &auth.project, request, now_ms)?;
        let(count,bytes):(i64,i64)=tx.query_row("SELECT count(*),COALESCE(sum(length(CAST(request_json AS BLOB))),0) FROM commands WHERE project_id=?1 AND target_run=?2 AND target_epoch=?3 AND state!='completed'",params![auth.project,request.target_run,sql_u64(request.target_epoch)?],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            count < MAX_PENDING_COMMANDS && bytes + body.len() as i64 <= MAX_PENDING_BYTES,
            "command queue is full"
        );
        let id = Uuid::new_v4().to_string();
        let sequence = append_event(
            &tx,
            &auth.project,
            "command-accepted",
            &auth.grant.principal,
            &serde_json::json!({"command_id":id,"request":request,"sender_run":auth.grant.run_id,"sender_epoch":auth.grant.run_epoch}),
            now_ms,
        )?;
        tx.execute("INSERT INTO commands(command_id,project_id,principal,idempotency_key,request_json,target_run,target_epoch,kind,payload_json,accepted_sequence,accepted_at,sender_run,sender_epoch,state,capability_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'accepted',?14)",params![id,auth.project,auth.grant.principal,request.idempotency_key,body,request.target_run,sql_u64(request.target_epoch)?,request.kind,json(&request.payload)?,sql_u64(sequence)?,now_ms,auth.grant.run_id,sql_u64(auth.grant.run_epoch)?,token_hash(token)])?;
        // The effect is committed in the same transaction as its receipt.
        tx.execute(
            "INSERT INTO effects(command_id,state) VALUES(?1,'pending')",
            [&id],
        )?;
        let accepted = receipt(&tx, &id)?;
        tx.commit()?;
        Ok(accepted)
    }

    /// Trusted engine lookup. Agent callers must pass through a scoped command
    /// endpoint; this method is not itself an authorization boundary.
    pub fn command_receipt(&self, owner: &OwnerFence, command_id: &str) -> Result<CommandReceipt> {
        assert_owner(&self.conn, owner)?;
        ensure!(
            receipt(&self.conn, command_id)?.project_id == owner.project_id,
            "command is outside project"
        );
        receipt(&self.conn, command_id)
    }

    pub fn agent_command_receipt(
        &self,
        token: &str,
        command_id: &str,
        now_ms: i64,
    ) -> Result<CommandReceipt> {
        let tx = self.conn.unchecked_transaction()?;
        let auth = authenticate(&tx, token, now_ms)?;
        let(project,principal,sender,epoch,body):(String,String,String,u64,String)=tx.query_row("SELECT project_id,principal,sender_run,sender_epoch,request_json FROM commands WHERE command_id=?1",[command_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,db_u64(row,3)?,row.get(4)?)))?;
        ensure!(
            project == auth.project
                && principal == auth.grant.principal
                && sender == auth.grant.run_id
                && epoch == auth.grant.run_epoch,
            "command receipt is outside sender attempt"
        );
        let request: CommandRequest = serde_json::from_str(&body)?;
        authorize(&tx, &auth, &format!("command:{}", request.kind))?;
        let accepted = receipt(&tx, command_id)?;
        tx.commit()?;
        Ok(accepted)
    }

    /// Atomic claim, serialized in acceptance order per target attempt. Returning
    /// None means completed, already claimed, or blocked by an earlier effect.
    /// An uncertain effect blocks later commands for the same attempt.
    pub fn claim_effect(
        &mut self,
        owner: &OwnerFence,
        command_id: &str,
        now_ms: i64,
    ) -> Result<Option<EffectClaim>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let (project,target,epoch,kind,payload,sequence,principal,sender_run,sender_epoch):(String,String,u64,String,String,u64,String,String,u64)=tx.query_row("SELECT project_id,target_run,target_epoch,kind,payload_json,accepted_sequence,principal,sender_run,sender_epoch FROM commands WHERE command_id=?1",[command_id],|r|Ok((r.get(0)?,r.get(1)?,db_u64(r,2)?,r.get(3)?,r.get(4)?,db_u64(r,5)?,r.get(6)?,r.get(7)?,db_u64(r,8)?)))?;
        ensure!(project == owner.project_id, "command is outside project");
        assert_run(&tx, &project, &target, epoch)?;
        let state: String = tx.query_row(
            "SELECT state FROM effects WHERE command_id=?1",
            [command_id],
            |r| r.get(0),
        )?;
        if state != "pending" {
            return Ok(None);
        }
        let blockers:i64=tx.query_row("SELECT count(*) FROM commands c JOIN effects e ON e.command_id=c.command_id WHERE c.project_id=?1 AND c.target_run=?2 AND c.target_epoch=?3 AND c.accepted_sequence<?4 AND e.state!='completed'",params![project,target,sql_u64(epoch)?,sql_u64(sequence)?],|r|r.get(0))?;
        if blockers != 0 {
            return Ok(None);
        }
        if matches!(kind.as_str(), "steer" | "interrupt" | "resume") {
            let encoded: String = tx.query_row("SELECT request_json FROM commands WHERE command_id=?1", [command_id], |row| row.get(0))?;
            let request: CommandRequest = serde_json::from_str(&encoded)?;
            let authorized = (|| -> Result<()> {
                let hash: Option<String> = tx.query_row("SELECT capability_hash FROM commands WHERE command_id=?1", [command_id], |row| row.get(0))?;
                let hash = hash.context("legacy control has no dispatch authority receipt")?;
                let auth = authenticate_hash(&tx, &hash, now_ms)?;
                ensure!(auth.project == project && auth.grant.principal == principal && auth.grant.run_id == sender_run && auth.grant.run_epoch == sender_epoch, "control sender authority changed");
                authorize(&tx, &auth, &format!("command:{kind}"))?;
                authorize_target(&tx, &auth, &target, epoch, &request.scope)?;
                super::runtime::validate_live_control(&tx, &project, &request, now_ms)
            })();
            if let Err(error) = authorized {
                let result = serde_json::json!({"status":"rejected","reason":"control authority or pinned backend turn is no longer current","detail":error.to_string()});
                complete(&tx, command_id, &result)?;
                append_event(&tx, &project, "effect-rejected", &owner.owner_id, &serde_json::json!({"command_id":command_id,"result":result}), now_ms)?;
                tx.commit()?;
                return Ok(None);
            }
        }
        let claim_id = Uuid::new_v4().to_string();
        tx.execute("UPDATE effects SET state='started',claim_id=?1,owner_epoch=?2,started_at=?3 WHERE command_id=?4",params![claim_id,sql_u64(owner.epoch)?,now_ms,command_id])?;
        tx.execute(
            "UPDATE commands SET state='started' WHERE command_id=?1",
            [command_id],
        )?;
        append_event(
            &tx,
            &project,
            "effect-started",
            &owner.owner_id,
            &serde_json::json!({"command_id":command_id,"claim_id":claim_id,"target_epoch":epoch,"owner_epoch":owner.epoch}),
            now_ms,
        )?;
        tx.commit()?;
        Ok(Some(EffectClaim {
            command_id: command_id.into(),
            claim_id,
            owner: owner.clone(),
            principal,
            sender_run,
            sender_epoch,
            target_run: target,
            target_epoch: epoch,
            kind,
            payload: serde_json::from_str(&payload)?,
        }))
    }

    pub fn finish_effect(
        &mut self,
        claim: &EffectClaim,
        result: &Value,
        now_ms: i64,
    ) -> Result<CommandReceipt> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, &claim.owner)?;
        assert_run(
            &tx,
            &claim.owner.project_id,
            &claim.target_run,
            claim.target_epoch,
        )?;
        let (stored_claim,stored_epoch,project,target,epoch,state):(Option<String>,Option<u64>,String,String,u64,String)=tx.query_row("SELECT e.claim_id,e.owner_epoch,c.project_id,c.target_run,c.target_epoch,e.state FROM effects e JOIN commands c ON c.command_id=e.command_id WHERE c.command_id=?1",[&claim.command_id],|r|Ok((r.get(0)?,db_optional_u64(r,1)?,r.get(2)?,r.get(3)?,db_u64(r,4)?,r.get(5)?)))?;
        ensure!(
            project == claim.owner.project_id
                && target == claim.target_run
                && epoch == claim.target_epoch
                && stored_epoch == Some(claim.owner.epoch)
                && stored_claim.as_deref() == Some(&claim.claim_id),
            "stale or mismatched effect claim"
        );
        if state == "completed" {
            let old = receipt(&tx, &claim.command_id)?;
            ensure!(
                old.result.as_ref().map(json).transpose()? == Some(json(result)?),
                "effect completion conflicts with original outcome"
            );
            return Ok(old);
        }
        ensure!(
            state == "started",
            "effect must be reconciled before completion"
        );
        complete(&tx, &claim.command_id, result)?;
        append_event(
            &tx,
            &project,
            "effect-completed",
            &claim.owner.owner_id,
            &serde_json::json!({"command_id":claim.command_id,"claim_id":claim.claim_id,"result":result}),
            now_ms,
        )?;
        let done = receipt(&tx, &claim.command_id)?;
        tx.commit()?;
        Ok(done)
    }

    /// Call after recovering the exclusive engine lock, never as a live polling
    /// operation. Opening SQLite alone does not recover or restart anything.
    pub fn recover(&mut self, owner: &OwnerFence, now_ms: i64) -> Result<Recovery> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let changed=tx.execute("UPDATE effects SET state='uncertain' WHERE state='started' AND command_id IN(SELECT command_id FROM commands WHERE project_id=?1)",[&owner.project_id])?;
        tx.execute(
            "UPDATE commands SET state='uncertain' WHERE project_id=?1 AND state='started'",
            [&owner.project_id],
        )?;
        if changed > 0 {
            append_event(
                &tx,
                &owner.project_id,
                "effects-recovery-uncertain",
                &owner.owner_id,
                &serde_json::json!({"count":changed}),
                now_ms,
            )?;
        }
        tx.commit()?;
        Ok(Recovery {
            pending: self.commands_in_state(owner, CommandState::Accepted, 1000)?,
            uncertain: self.commands_in_state(owner, CommandState::Uncertain, 1000)?,
        })
    }

    pub fn commands_in_state(
        &self,
        owner: &OwnerFence,
        state: CommandState,
        limit: usize,
    ) -> Result<Vec<CommandReceipt>> {
        assert_owner(&self.conn, owner)?;
        ensure!((1..=1000).contains(&limit), "command page must be 1..1000");
        let mut stmt=self.conn.prepare("SELECT command_id FROM commands WHERE project_id=?1 AND state=?2 ORDER BY accepted_sequence LIMIT ?3")?;
        let ids = stmt
            .query_map(
                params![owner.project_id, state.as_str(), limit as i64],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter().map(|id| receipt(&self.conn, &id)).collect()
    }

    /// Scheduler projection, distinct from recovery/history. Return at most
    /// one ready head per current target attempt. A page of stale or blocked
    /// commands cannot starve unrelated ready targets.
    pub fn ready_commands(&self, owner: &OwnerFence, limit: usize) -> Result<Vec<CommandReceipt>> {
        assert_owner(&self.conn, owner)?;
        ensure!(
            (1..=1000).contains(&limit),
            "ready command page must be 1..1000"
        );
        let mut stmt=self.conn.prepare("SELECT c.command_id FROM commands c JOIN effects e ON e.command_id=c.command_id JOIN runs r ON r.project_id=c.project_id AND r.run_id=c.target_run AND r.epoch=c.target_epoch WHERE c.project_id=?1 AND c.state='accepted' AND e.state='pending' AND NOT EXISTS(SELECT 1 FROM commands previous WHERE previous.project_id=c.project_id AND previous.target_run=c.target_run AND previous.target_epoch=c.target_epoch AND previous.accepted_sequence<c.accepted_sequence AND previous.state!='completed') ORDER BY c.accepted_sequence LIMIT ?2")?;
        let ids = stmt
            .query_map(params![owner.project_id, limit as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter().map(|id| receipt(&self.conn, &id)).collect()
    }

    pub fn reconcile_effect(
        &mut self,
        owner: &OwnerFence,
        command_id: &str,
        resolution: &Reconciliation,
        now_ms: i64,
    ) -> Result<CommandReceipt> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let (project, target, epoch, state): (String, String, u64, String) = tx.query_row(
            "SELECT project_id,target_run,target_epoch,state FROM commands WHERE command_id=?1",
            [command_id],
            |r| Ok((r.get(0)?, r.get(1)?, db_u64(r, 2)?, r.get(3)?)),
        )?;
        ensure!(
            project == owner.project_id && state == "uncertain",
            "only uncertain project effects can be reconciled"
        );
        match resolution {
            Reconciliation::Completed { result, evidence } => {
                ensure!(!evidence.is_null(), "reconciliation requires evidence");
                complete(&tx, command_id, result)?;
            }
            Reconciliation::RetrySafe { evidence } => {
                ensure!(!evidence.is_null(), "safe retry requires evidence");
                assert_run(&tx, &project, &target, epoch)?;
                tx.execute("UPDATE effects SET state='pending',claim_id=NULL,owner_epoch=NULL,started_at=NULL WHERE command_id=?1",[command_id])?;
                tx.execute(
                    "UPDATE commands SET state='accepted' WHERE command_id=?1",
                    [command_id],
                )?;
            }
        }
        append_event(
            &tx,
            &project,
            "effect-reconciled",
            &owner.owner_id,
            &serde_json::json!({"command_id":command_id,"resolution":resolution}),
            now_ms,
        )?;
        let reconciled = receipt(&tx, command_id)?;
        tx.commit()?;
        Ok(reconciled)
    }
}

pub(super) fn receipt(conn: &Connection, id: &str) -> Result<CommandReceipt> {
    let(project_id,accepted_sequence,accepted_at_ms,state,result):(String,u64,i64,String,Option<String>)=conn.query_row("SELECT project_id,accepted_sequence,accepted_at,state,result_json FROM commands WHERE command_id=?1",[id],|r|Ok((r.get(0)?,db_u64(r,1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    Ok(CommandReceipt {
        command_id: id.into(),
        project_id,
        accepted_sequence,
        accepted_at_ms,
        state: CommandState::parse(&state)?,
        result: result.map(|s| serde_json::from_str(&s)).transpose()?,
    })
}
/// A retired attempt can never execute a still-pending effect. Complete those
/// receipts in the same transaction as its epoch change. Started/uncertain
/// effects remain for reconciliation; they are never relabeled as unexecuted.
pub(super) fn retire_pending_targets(tx: &Transaction<'_>, project: &str, run: Option<&str>, principal: &str, now_ms: i64) -> Result<()> {
    let changed = tx.execute(
        "UPDATE effects SET state='completed' WHERE state='pending' AND claim_id IS NULL AND command_id IN (SELECT c.command_id FROM commands c JOIN runs r ON r.project_id=c.project_id AND r.run_id=c.target_run WHERE c.project_id=?1 AND (?2 IS NULL OR c.target_run=?2) AND c.state='accepted' AND c.target_epoch!=r.epoch)",
        params![project,run],
    )?;
    if changed == 0 { return Ok(()); }
    let result = serde_json::json!({"status":"rejected","reason":"target attempt retired before dispatch"});
    let completed = tx.execute(
        "UPDATE commands SET state='completed',result_json=?3 WHERE project_id=?1 AND (?2 IS NULL OR target_run=?2) AND state='accepted' AND target_epoch!=(SELECT epoch FROM runs WHERE runs.project_id=commands.project_id AND runs.run_id=commands.target_run) AND command_id IN(SELECT command_id FROM effects WHERE state='completed' AND claim_id IS NULL)",
        params![project,run,json(&result)?],
    )?;
    ensure!(changed == completed, "retired command/effect count differs");
    append_event(tx,project,"pending-target-effects-retired",principal,&serde_json::json!({"run_id":run,"count":completed,"result":result}),now_ms)?;
    Ok(())
}

fn complete(tx: &Transaction<'_>, id: &str, result: &Value) -> Result<()> {
    tx.execute(
        "UPDATE effects SET state='completed' WHERE command_id=?1",
        [id],
    )?;
    tx.execute(
        "UPDATE commands SET state='completed',result_json=?1 WHERE command_id=?2",
        params![json(result)?, id],
    )?;
    Ok(())
}
