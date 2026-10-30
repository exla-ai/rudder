use super::*;
use crate::protocol::ObjectKind;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    pub idempotency_key: String,
    pub topic: String,
    /// Zero represents a topic with no accepted decision yet.
    pub expected_version: u64,
    pub scope: String,
    pub statement: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcceptedDecision {
    pub decision_id: String,
    pub project_id: String,
    pub topic: String,
    pub version: u64,
    pub author: String,
    pub scope: String,
    pub statement: Value,
    pub supersedes: Option<String>,
    pub sequence: u64,
}

/// Immutable evidence references must already have been verified/pinned by the
/// content store and the canonical gate runner. This journal validates their
/// shape, kinds and tested head; it cannot prove a test ran from JSON alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationEvidence {
    pub tested_head: String,
    pub tested_source_root: ObjectRef,
    pub tested_environment_root: ObjectRef,
    pub action_results: Vec<ObjectRef>,
    pub checks: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationRequest {
    pub idempotency_key: String,
    pub expected_head: String,
    pub new_head: String,
    pub proposed_source_root: ObjectRef,
    pub proposed_environment_root: ObjectRef,
    /// Complete relevant decision AND build-policy version vector. Computing
    /// this dependency closure is the gate planner's job, not inferred here.
    pub versions: TopicVersions,
    pub evidence: IntegrationEvidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationReceipt {
    pub integration_id: String,
    pub project_id: String,
    pub previous_head: String,
    pub new_head: String,
    pub owner_epoch: u64,
    pub proposed_source_root: ObjectRef,
    pub proposed_environment_root: ObjectRef,
    pub versions: TopicVersions,
    pub evidence: IntegrationEvidence,
    pub sequence: u64,
}

impl Journal {
    pub fn topic_versions(
        &self,
        project: &str,
        topics: &BTreeSet<String>,
    ) -> Result<TopicVersions> {
        // Pin the entire vector to one read snapshot, including absent topics.
        let tx = self.conn.unchecked_transaction()?;
        project_state(&tx, project)?;
        let versions = topics
            .iter()
            .map(|topic| Ok((topic.clone(), topic_version(&tx, project, topic)?)))
            .collect();
        tx.commit()?;
        versions
    }

    pub fn accept_decision(
        &mut self,
        token: &str,
        request: &DecisionRequest,
        now_ms: i64,
    ) -> Result<AcceptedDecision> {
        valid_id(&request.idempotency_key)?;
        valid_id(&request.topic)?;
        let body = json(request)?;
        ensure!(body.len() <= 256 * 1024, "decision exceeds payload limit");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "decision:accept")?;
        ensure!(
            auth.grant.topics.contains(&request.topic)
                && auth.grant.scopes.contains(&request.scope),
            "decision is outside delegated authority"
        );
        let duplicate:Option<(String,String)>=tx.query_row("SELECT decision_id,request_json FROM decisions WHERE project_id=?1 AND principal=?2 AND idempotency_key=?3",params![auth.project,auth.grant.principal,request.idempotency_key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((id, original)) = duplicate {
            ensure!(
                original == body,
                "idempotency key reused with a different decision"
            );
            return decision(&tx, &id);
        }
        let current = topic_version(&tx, &auth.project, &request.topic)?;
        ensure!(
            current == request.expected_version,
            "accepted topic version changed"
        );
        let version = current.checked_add(1).context("topic version exhausted")?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT decision_id FROM topics WHERE project_id=?1 AND topic=?2",
                params![auth.project, request.topic],
                |r| r.get(0),
            )
            .optional()?;
        let id = Uuid::new_v4().to_string();
        let sequence = append_event(
            &tx,
            &auth.project,
            "decision-accepted",
            &auth.grant.principal,
            &serde_json::json!({"decision_id":id,"request":request,"version":version,"supersedes":previous}),
            now_ms,
        )?;
        tx.execute("INSERT INTO decisions(decision_id,project_id,topic,version,principal,scope,statement_json,previous_id,sequence,idempotency_key,request_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![id,auth.project,request.topic,sql_u64(version)?,auth.grant.principal,request.scope,json(&request.statement)?,previous,sql_u64(sequence)?,request.idempotency_key,body])?;
        tx.execute("INSERT INTO topics(project_id,topic,version,decision_id) VALUES(?1,?2,?3,?4) ON CONFLICT(project_id,topic) DO UPDATE SET version=excluded.version,decision_id=excluded.decision_id",params![auth.project,request.topic,sql_u64(version)?,id])?;
        let accepted = decision(&tx, &id)?;
        tx.commit()?;
        Ok(accepted)
    }

    /// Trusted authority projection; agent-facing readers require separate
    /// capability-based filtering rather than this raw project-wide accessor.
    pub fn current_decision(&self, project: &str, topic: &str) -> Result<Option<AcceptedDecision>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT decision_id FROM topics WHERE project_id=?1 AND topic=?2",
                params![project, topic],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| decision(&self.conn, &id)).transpose()
    }

    pub fn read_topics(
        &self,
        token: &str,
        topics: &BTreeSet<String>,
        now_ms: i64,
    ) -> Result<BTreeMap<String, Option<AcceptedDecision>>> {
        let tx = self.conn.unchecked_transaction()?;
        let auth = authenticate(&tx, token, now_ms)?;
        authorize(&tx, &auth, "topics:read")?;
        ensure!(
            topics.iter().all(|topic| auth.grant.topics.contains(topic)),
            "topic is outside capability"
        );
        let mut result = BTreeMap::new();
        for topic in topics {
            let id: Option<String> = tx
                .query_row(
                    "SELECT decision_id FROM topics WHERE project_id=?1 AND topic=?2",
                    params![auth.project, topic],
                    |row| row.get(0),
                )
                .optional()?;
            result.insert(topic.clone(), id.map(|id| decision(&tx, &id)).transpose()?);
        }
        tx.commit()?;
        Ok(result)
    }

    /// Final integration fence. Head, authority epoch and every relevant topic
    /// version are compared in the SAME write transaction as head advancement.
    pub fn integrate(
        &mut self,
        owner: &OwnerFence,
        request: &IntegrationRequest,
        now_ms: i64,
    ) -> Result<IntegrationReceipt> {
        validate_integration_shape(request)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = integration_duplicate(&tx, &owner.project_id, request)? {
            return Ok(receipt);
        }
        super::vcs::assert_legacy_key_unreserved(&tx, &owner.project_id, &request.idempotency_key)?;
        validate_integration_gate(&tx, owner, request, None)?;
        let receipt = apply_integration(&tx, owner, request, now_ms)?;
        tx.commit()?;
        Ok(receipt)
    }
}

pub(super) fn validate_integration_shape(request: &IntegrationRequest) -> Result<()> {
    valid_id(&request.idempotency_key)?;
    valid_id(&request.expected_head)?;
    valid_id(&request.new_head)?;
    ensure!(
        !request.evidence.checks.is_null(),
        "integration requires verification evidence"
    );
    for (reference, kind) in [
        (&request.proposed_source_root, ObjectKind::SourceRoot),
        (&request.evidence.tested_source_root, ObjectKind::SourceRoot),
        (
            &request.proposed_environment_root,
            ObjectKind::EnvironmentRoot,
        ),
        (
            &request.evidence.tested_environment_root,
            ObjectKind::EnvironmentRoot,
        ),
    ] {
        reference.validate()?;
        ensure!(reference.kind == kind, "incorrect integration root kind");
    }
    for reference in &request.evidence.action_results {
        reference.validate()?;
        ensure!(
            reference.kind == ObjectKind::ActionResult,
            "integration result must be an action result"
        );
    }
    let body = json(request)?;
    ensure!(
        body.len() <= 1024 * 1024,
        "integration evidence exceeds size limit"
    );
    ensure!(
        request.evidence.tested_head == request.expected_head,
        "verification evidence belongs to another integration base"
    );
    ensure!(
        request.evidence.tested_source_root == request.proposed_source_root,
        "tested source root differs from proposed source"
    );
    ensure!(
        request.evidence.tested_environment_root == request.proposed_environment_root,
        "tested environment root differs from proposed environment"
    );
    Ok(())
}

pub(super) fn integration_duplicate(
    conn: &Connection,
    project: &str,
    request: &IntegrationRequest,
) -> Result<Option<IntegrationReceipt>> {
    let duplicate: Option<(String, String)> = conn.query_row(
        "SELECT receipt_json,request_json FROM integrations WHERE project_id=?1 AND idempotency_key=?2",
        params![project, request.idempotency_key], |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    if let Some((receipt, body)) = duplicate {
        ensure!(
            body == json(request)?,
            "idempotency key reused with a different integration"
        );
        return Ok(Some(serde_json::from_str(&receipt)?));
    }
    Ok(None)
}

pub(super) fn validate_integration_gate(
    conn: &Connection,
    owner: &OwnerFence,
    request: &IntegrationRequest,
    except_intent: Option<&str>,
) -> Result<()> {
    assert_owner(conn, owner)?;
    super::vcs::assert_no_uncertain_integration(conn, &owner.project_id, except_intent)?;
    ensure!(
        project_state(conn, &owner.project_id)?.integration_head == request.expected_head,
        "integration head changed"
    );
    for (topic, expected) in &request.versions {
        ensure!(
            topic_version(conn, &owner.project_id, topic)? == *expected,
            "decision or build-policy version changed: {topic}"
        );
    }
    Ok(())
}

pub(super) fn apply_integration(
    tx: &Transaction<'_>,
    owner: &OwnerFence,
    request: &IntegrationRequest,
    now_ms: i64,
) -> Result<IntegrationReceipt> {
    let body = json(request)?;
    let id = Uuid::new_v4().to_string();
    let sequence = append_event(
        &tx,
        &owner.project_id,
        "integration-advanced",
        &owner.owner_id,
        &serde_json::json!({"integration_id":id,"owner_epoch":owner.epoch,"request":request}),
        now_ms,
    )?;
    let receipt = IntegrationReceipt {
        integration_id: id.clone(),
        project_id: owner.project_id.clone(),
        previous_head: request.expected_head.clone(),
        new_head: request.new_head.clone(),
        owner_epoch: owner.epoch,
        proposed_source_root: request.proposed_source_root.clone(),
        proposed_environment_root: request.proposed_environment_root.clone(),
        versions: request.versions.clone(),
        evidence: request.evidence.clone(),
        sequence,
    };
    tx.execute(
        "UPDATE projects SET integration_head=?1 WHERE project_id=?2",
        params![request.new_head, owner.project_id],
    )?;
    tx.execute("INSERT INTO integrations(integration_id,project_id,previous_head,new_head,owner_epoch,versions_json,evidence_json,sequence,idempotency_key,request_json,receipt_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![id,owner.project_id,request.expected_head,request.new_head,sql_u64(owner.epoch)?,json(&request.versions)?,json(&request.evidence)?,sql_u64(sequence)?,request.idempotency_key,body,json(&receipt)?])?;
    Ok(receipt)
}

fn topic_version(conn: &Connection, project: &str, topic: &str) -> Result<u64> {
    Ok(conn
        .query_row(
            "SELECT version FROM topics WHERE project_id=?1 AND topic=?2",
            params![project, topic],
            |r| db_u64(r, 0),
        )
        .optional()?
        .unwrap_or(0))
}
fn decision(conn: &Connection, id: &str) -> Result<AcceptedDecision> {
    let(project_id,topic,version,author,scope,statement,supersedes,sequence):(String,String,u64,String,String,String,Option<String>,u64)=conn.query_row("SELECT project_id,topic,version,principal,scope,statement_json,previous_id,sequence FROM decisions WHERE decision_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,db_u64(r,2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,db_u64(r,7)?)))?;
    Ok(AcceptedDecision {
        decision_id: id.into(),
        project_id,
        topic,
        version,
        author,
        scope,
        statement: serde_json::from_str(&statement)?,
        supersedes,
        sequence,
    })
}
