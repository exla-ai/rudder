//! Actor-owned observations. Discovery is informational; a live control pins
//! both thread and turn inside the same transaction as durable acceptance.
use super::*;

pub const RUNTIME_MAX_AGE_MS: i64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    Starting,
    Working,
    Waiting,
    Interrupted,
    Completed,
    Failed,
    Closed,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeObservation {
    pub run_id: String,
    pub run_epoch: u64,
    pub attempt_id: String,
    pub backend: String,
    pub execution_profile: String,
    pub thread_id: Option<String>,
    pub active_turn_id: Option<String>,
    pub last_interrupted_turn_id: Option<String>,
    pub phase: RuntimePhase,
    pub capabilities: BTreeSet<String>,
    /// Ownership by the actor, not a promise about kernel process liveness.
    pub pty_owned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    #[serde(flatten)]
    pub observation: RuntimeObservation,
    pub owner_epoch: u64,
    pub observed_at_ms: i64,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredAgent {
    #[serde(flatten)]
    pub identity: AgentIdentity,
    pub runtime: Option<RuntimeSnapshot>,
}

impl Journal {
    pub fn observe_runtime(
        &mut self,
        owner: &OwnerFence,
        observation: &RuntimeObservation,
        now_ms: i64,
    ) -> Result<RuntimeSnapshot> {
        let mut snapshots =
            self.observe_runtimes(owner, std::slice::from_ref(observation), now_ms)?;
        Ok(snapshots.remove(0))
    }

    /// One durable owner transaction for a heartbeat, not one fsync per live
    /// agent. The whole batch rolls back if any attempt fence is stale.
    pub fn observe_runtimes(
        &mut self,
        owner: &OwnerFence,
        observations: &[RuntimeObservation],
        now_ms: i64,
    ) -> Result<Vec<RuntimeSnapshot>> {
        ensure!(
            observations.len() <= 4096,
            "runtime observation batch exceeds4096 entries"
        );
        for observation in observations {
            validate_observation(observation)?;
        }
        if observations.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let mut snapshots = Vec::with_capacity(observations.len());
        for observation in observations {
            assert_run(
                &tx,
                &owner.project_id,
                &observation.run_id,
                observation.run_epoch,
            )?;
            let instance: Option<String> = tx.query_row(
                "SELECT instance_id FROM runs WHERE project_id=?1 AND run_id=?2",
                params![owner.project_id, observation.run_id],
                |row| row.get(0),
            )?;
            ensure!(
                instance.as_deref() == Some(observation.attempt_id.as_str()),
                "runtime belongs to a replaced attempt"
            );
            let encoded = json(observation)?;
            let old: Option<(String,u64)> = tx.query_row("SELECT observation_json,owner_epoch FROM runtime_observations WHERE project_id=?1 AND run_id=?2",params![owner.project_id,observation.run_id],|row|Ok((row.get(0)?,db_u64(row,1)?))).optional()?;
            tx.execute("INSERT INTO runtime_observations(project_id,run_id,run_epoch,owner_epoch,instance_id,observation_json,observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(project_id,run_id) DO UPDATE SET run_epoch=excluded.run_epoch,owner_epoch=excluded.owner_epoch,instance_id=excluded.instance_id,observation_json=excluded.observation_json,observed_at=excluded.observed_at",params![owner.project_id,observation.run_id,sql_u64(observation.run_epoch)?,sql_u64(owner.epoch)?,observation.attempt_id,encoded,now_ms])?;
            if old.as_ref() != Some(&(encoded, owner.epoch)) {
                append_event(
                    &tx,
                    &owner.project_id,
                    "runtime-observed",
                    &owner.owner_id,
                    &serde_json::json!({"owner_epoch":owner.epoch,"observation":observation}),
                    now_ms,
                )?;
            }

            snapshots.push(RuntimeSnapshot {
                observation: observation.clone(),
                owner_epoch: owner.epoch,
                observed_at_ms: now_ms,
                stale: false,
            });
        }
        tx.commit()?;
        Ok(snapshots)
    }

    pub fn runtime_snapshot(
        &self,
        owner: &OwnerFence,
        run_id: &str,
        now_ms: i64,
    ) -> Result<Option<RuntimeSnapshot>> {
        assert_owner(&self.conn, owner)?;
        runtime_snapshot(&self.conn, &owner.project_id, run_id, owner.epoch, now_ms)
    }

    pub fn find_agent_runtime(&self, token: &str, now_ms: i64) -> Result<Vec<DiscoveredAgent>> {
        let identities = self.find_agents(token, now_ms)?;
        let auth = self.authenticate_agent(token, now_ms)?;
        identities
            .into_iter()
            .map(|identity| {
                let runtime = runtime_snapshot(
                    &self.conn,
                    &identity.project_id,
                    &identity.run_id,
                    auth.owner_epoch,
                    now_ms,
                )?
                .filter(|snapshot| snapshot.observation.run_epoch == identity.run_epoch);
                Ok(DiscoveredAgent { identity, runtime })
            })
            .collect()
    }

    /// A lost/expired external write remains blocking. This never re-enqueues
    /// it and can annotate an old target attempt after that target was replaced.
    pub fn mark_effect_uncertain(
        &mut self,
        claim: &EffectClaim,
        evidence: &Value,
        now_ms: i64,
    ) -> Result<CommandReceipt> {
        ensure!(!evidence.is_null(), "uncertainty requires evidence");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, &claim.owner)?;
        let (project,target,epoch,id,owner_epoch,state):(String,String,u64,Option<String>,Option<u64>,String)=tx.query_row("SELECT c.project_id,c.target_run,c.target_epoch,e.claim_id,e.owner_epoch,e.state FROM commands c JOIN effects e ON e.command_id=c.command_id WHERE c.command_id=?1",[&claim.command_id],|row|Ok((row.get(0)?,row.get(1)?,db_u64(row,2)?,row.get(3)?,db_optional_u64(row,4)?,row.get(5)?)))?;
        ensure!(
            project == claim.owner.project_id
                && target == claim.target_run
                && epoch == claim.target_epoch
                && id.as_deref() == Some(claim.claim_id.as_str())
                && owner_epoch == Some(claim.owner.epoch),
            "stale or mismatched uncertain claim"
        );
        if state == "completed" || state == "uncertain" {
            return super::commands::receipt(&tx, &claim.command_id);
        }
        ensure!(
            state == "started",
            "only a started effect may become uncertain"
        );
        tx.execute(
            "UPDATE effects SET state='uncertain' WHERE command_id=?1",
            [&claim.command_id],
        )?;
        tx.execute(
            "UPDATE commands SET state='uncertain',result_json=?1 WHERE command_id=?2",
            params![
                json(&serde_json::json!({"status":"uncertain","evidence":evidence}))?,
                claim.command_id
            ],
        )?;
        append_event(
            &tx,
            &project,
            "effect-uncertain",
            &claim.owner.owner_id,
            &serde_json::json!({"command_id":claim.command_id,"claim_id":claim.claim_id,"evidence":evidence}),
            now_ms,
        )?;
        let receipt = super::commands::receipt(&tx, &claim.command_id)?;
        tx.commit()?;
        Ok(receipt)
    }
}

fn validate_observation(observation: &RuntimeObservation) -> Result<()> {
    for value in [
        &observation.run_id,
        &observation.attempt_id,
        &observation.backend,
        &observation.execution_profile,
    ] {
        valid_id(value)?;
    }
    for value in [
        &observation.thread_id,
        &observation.active_turn_id,
        &observation.last_interrupted_turn_id,
    ]
    .into_iter()
    .flatten()
    {
        valid_id(value)?;
    }
    ensure!(observation.run_epoch > 0, "invalid runtime epoch");
    ensure!(
        observation.capabilities.iter().all(|kind| matches!(
            kind.as_str(),
            "inspect" | "deliver" | "steer" | "interrupt" | "resume" | "cancel"
        )),
        "invalid runtime capability"
    );
    if !observation.pty_owned || observation.phase == RuntimePhase::Uncertain {
        ensure!(
            observation
                .capabilities
                .iter()
                .all(|kind| kind == "inspect" || observation.pty_owned && kind == "cancel"),
            "inactive runtime cannot advertise live controls"
        );
    }
    if observation.phase == RuntimePhase::Working {
        ensure!(
            observation.thread_id.is_some()
                && observation.active_turn_id.is_some()
                && observation.pty_owned,
            "working runtime requires an owned active turn"
        );
    }
    if observation.phase == RuntimePhase::Interrupted {
        ensure!(
            observation.thread_id.is_some()
                && observation.last_interrupted_turn_id.is_some()
                && observation.active_turn_id.is_none(),
            "interrupted runtime requires exact stopped-turn evidence"
        );
    }
    Ok(())
}

fn runtime_snapshot(
    conn: &Connection,
    project: &str,
    run_id: &str,
    owner_epoch: u64,
    now_ms: i64,
) -> Result<Option<RuntimeSnapshot>> {
    let stored: Option<(String,i64)> = conn.query_row("SELECT o.observation_json,o.observed_at FROM runtime_observations o JOIN runs r ON r.project_id=o.project_id AND r.run_id=o.run_id JOIN projects p ON p.project_id=o.project_id WHERE o.project_id=?1 AND o.run_id=?2 AND o.owner_epoch=?3 AND p.owner_active=1 AND p.owner_epoch=o.owner_epoch AND r.epoch=o.run_epoch AND r.instance_id=o.instance_id",params![project,run_id,sql_u64(owner_epoch)?],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    stored
        .map(|(json, observed_at_ms)| {
            Ok(RuntimeSnapshot {
                observation: serde_json::from_str(&json)?,
                owner_epoch,
                observed_at_ms,
                stale: now_ms
                    .checked_sub(observed_at_ms)
                    .is_none_or(|age| !(0..=RUNTIME_MAX_AGE_MS).contains(&age)),
            })
        })
        .transpose()
}

pub(super) fn validate_live_control(
    conn: &Connection,
    project: &str,
    request: &CommandRequest,
    now_ms: i64,
) -> Result<()> {
    if !matches!(request.kind.as_str(), "steer" | "interrupt" | "resume") {
        return Ok(());
    }
    let expected_thread = request
        .payload
        .get("expected_thread_id")
        .and_then(Value::as_str)
        .context("live control requires expected_thread_id")?;
    let expected_turn = request
        .payload
        .get("expected_turn_id")
        .and_then(Value::as_str)
        .context("live control requires expected_turn_id")?;
    valid_id(expected_thread)?;
    valid_id(expected_turn)?;
    let owner_epoch: u64 = conn.query_row(
        "SELECT owner_epoch FROM projects WHERE project_id=?1",
        [project],
        |row| db_u64(row, 0),
    )?;
    let snapshot = runtime_snapshot(conn, project, &request.target_run, owner_epoch, now_ms)?
        .context("target has no current actor runtime observation")?;
    let observation = &snapshot.observation;
    ensure!(
        !snapshot.stale && observation.run_epoch == request.target_epoch && observation.pty_owned,
        "target runtime is stale or not owned"
    );
    ensure!(
        observation.thread_id.as_deref() == Some(expected_thread),
        "expected thread is no longer current"
    );
    ensure!(
        observation.capabilities.contains(&request.kind),
        "target execution profile does not support this control"
    );
    if request.kind == "resume" {
        ensure!(
            observation.phase == RuntimePhase::Interrupted
                && observation.active_turn_id.is_none()
                && observation.last_interrupted_turn_id.as_deref() == Some(expected_turn),
            "resume requires the exact observed interrupted turn"
        );
    } else {
        ensure!(
            observation.phase == RuntimePhase::Working
                && observation.active_turn_id.as_deref() == Some(expected_turn),
            "control requires the exact observed active turn"
        );
    }
    Ok(())
}
