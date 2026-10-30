//! Durable intent around an exact external jj operation publication.
//!
//! The trusted caller holds the private integration-repository OS lock. Execute
//! these methods off the App actor. The publisher performs bounded local VCS
//! work, never a model/network call. It must validate the exact bridge record;
//! a guest must not implement this trusted interface or access this journal.

use super::*;
use crate::protocol::{canonical_json, ObjectKind};
use crate::store::LocalStore;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcsRunFence {
    pub run_id: String,
    pub run_epoch: u64,
    pub attempt_id: String,
    pub view_id: String,
}

/// The durable accepted intent precedes jj Transaction::write. A target commit
/// and op ID are attached only after preparation. Verification evidence binds
/// the already sealed versioned candidate, independent of its Git encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcsIntentRequest {
    pub idempotency_key: String,
    pub source_run: VcsRunFence,
    pub expected_head: String,
    pub proposed_source_root: ObjectRef,
    pub proposed_environment_root: ObjectRef,
    pub recovery_root: ObjectRef,
    pub seed_root: ObjectRef,
    pub projection_policy: ObjectRef,
    pub profile: ObjectRef,
    pub expected_view_root: Option<ObjectRef>,
    pub hardlinks: Option<ObjectRef>,
    pub versions: TopicVersions,
    pub evidence: IntegrationEvidence,
}
impl VcsIntentRequest {
    fn integration(&self, head: &str) -> IntegrationRequest {
        IntegrationRequest {
            idempotency_key: self.idempotency_key.clone(),
            expected_head: self.expected_head.clone(),
            new_head: head.into(),
            proposed_source_root: self.proposed_source_root.clone(),
            proposed_environment_root: self.proposed_environment_root.clone(),
            versions: self.versions.clone(),
            evidence: self.evidence.clone(),
        }
    }
    fn validate(&self) -> Result<()> {
        super::decisions::validate_integration_shape(&self.integration("prepared-later"))?;
        for id in [
            &self.source_run.run_id,
            &self.source_run.attempt_id,
            &self.source_run.view_id,
        ] {
            valid_id(id)?;
        }
        ensure!(
            self.source_run.run_epoch > 0,
            "source run epoch must be positive"
        );
        raw_hex(&self.expected_head, 40)?;
        self.recovery_root.validate()?;
        ensure!(
            matches!(
                self.recovery_root.kind,
                ObjectKind::Checkpoint | ObjectKind::RunnableRoot
            ),
            "invalid recovery root kind"
        );
        kind(&self.seed_root, ObjectKind::Directory)?;
        kind(&self.projection_policy, ObjectKind::Blob)?;
        kind(&self.profile, ObjectKind::Blob)?;
        if let Some(view) = &self.expected_view_root {
            kind(view, ObjectKind::Directory)?;
        }
        if let Some(hardlinks) = &self.hardlinks {
            kind(hardlinks, ObjectKind::Blob)?;
        }
        ensure!(
            json(self)?.len() <= 1024 * 1024,
            "VCS intent exceeds size limit"
        );
        Ok(())
    }
    fn roots(&self) -> Vec<ObjectRef> {
        let mut roots = vec![
            self.proposed_source_root.clone(),
            self.proposed_environment_root.clone(),
            self.recovery_root.clone(),
            self.seed_root.clone(),
            self.projection_policy.clone(),
            self.profile.clone(),
        ];
        roots.extend(self.expected_view_root.clone());
        roots.extend(self.hardlinks.clone());
        roots.extend(self.evidence.action_results.clone());
        roots.sort();
        roots.dedup();
        roots
    }
}

/// A cycle-free envelope for the exact canonical PreparedJjOperation Blob.
/// The trusted bridge also verifies its actual op tags, parents, commit/change
/// identity and merged tree. These fields never authorize publication alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedVcsOperation {
    pub format_version: u32,
    pub record: ObjectRef,
    pub operation_id: String,
    pub expected_operation_heads: Vec<String>,
    pub commit_id: String,
    pub proof_identity: ObjectRef,
}
impl PreparedVcsOperation {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported VCS preparation envelope"
        );
        kind(&self.record, ObjectKind::Blob)?;
        kind(&self.proof_identity, ObjectKind::Blob)?;
        ensure!(
            self.record.size_bytes <= 1024 * 1024 && self.proof_identity.size_bytes <= 1024 * 1024,
            "VCS preparation exceeds size limit"
        );
        raw_hex(&self.operation_id, 128)?;
        raw_hex(&self.commit_id, 40)?;
        ensure!(
            !self.expected_operation_heads.is_empty()
                && self.expected_operation_heads.len() <= 1024,
            "invalid previous operation head count"
        );
        for head in &self.expected_operation_heads {
            raw_hex(head, 128)?;
            ensure!(
                head != &self.operation_id,
                "prepared operation cannot replace itself"
            );
        }
        ensure!(
            self.expected_operation_heads
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                == self.expected_operation_heads.len(),
            "duplicate previous operation head"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsIntentState {
    Accepted,
    Prepared,
    Started,
    Uncertain,
    Published,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcsIntentReceipt {
    pub intent_id: String,
    pub project_id: String,
    pub original_owner: OwnerFence,
    pub request: VcsIntentRequest,
    pub state: VcsIntentState,
    pub prepared: Option<PreparedVcsOperation>,
    pub retention_pins: BTreeMap<String, ObjectRef>,
    pub result: Option<Value>,
    pub integration: Option<IntegrationReceipt>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VcsPublicationObservation {
    /// Exact complete expected head set still holds under the private repo lock.
    NotPublished,
    /// Exact prepared operation is the current complete op-head set.
    Published,
    HeadChanged {
        actual_heads: Vec<String>,
    },
    Uncertain {
        message: String,
    },
}

pub trait VcsPublisher {
    fn inspect(&mut self, prepared: &PreparedVcsOperation) -> Result<VcsPublicationObservation>;
    /// At most once after durable Started, while the final journal fence is held.
    fn publish(&mut self, prepared: &PreparedVcsOperation) -> Result<VcsPublicationObservation>;
}

impl Journal {
    pub fn begin_vcs_intent(
        &mut self,
        owner: &OwnerFence,
        store: &LocalStore,
        request: &VcsIntentRequest,
        now_ms: i64,
    ) -> Result<VcsIntentReceipt> {
        request.validate()?;
        let body = json(request)?;
        if let Some(receipt) = duplicate(
            &self.conn,
            &owner.project_id,
            &request.idempotency_key,
            &body,
        )? {
            return Ok(receipt);
        }
        assert_integration_key_available(&self.conn, &owner.project_id, &request.idempotency_key)?;
        super::decisions::validate_integration_gate(
            &self.conn,
            owner,
            &request.integration("prepared-later"),
            None,
        )?;
        source_fence(&self.conn, &owner.project_id, &request.source_run)?;
        validate_recovery(store, request)?;
        let id = Uuid::new_v4().to_string();
        // Retention precedes journal publication. A crash/failed fence may leak
        // conservative pins; it cannot leave a committed intent with no roots.
        let pins = retain(store, &id, request.roots())?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = duplicate(&tx, &owner.project_id, &request.idempotency_key, &body)? {
            return Ok(receipt);
        }
        super::decisions::validate_integration_gate(
            &tx,
            owner,
            &request.integration("prepared-later"),
            None,
        )?;
        source_fence(&tx, &owner.project_id, &request.source_run)?;
        assert_integration_key_available(&tx, &owner.project_id, &request.idempotency_key)?;
        let sequence = append_event(
            &tx,
            &owner.project_id,
            "vcs-intent-accepted",
            &owner.owner_id,
            &serde_json::json!({"intent_id":id,"request":request}),
            now_ms,
        )?;
        let receipt = VcsIntentReceipt {
            intent_id: id.clone(),
            project_id: owner.project_id.clone(),
            original_owner: owner.clone(),
            request: request.clone(),
            state: VcsIntentState::Accepted,
            prepared: None,
            retention_pins: pins,
            result: None,
            integration: None,
            sequence,
        };
        tx.execute("INSERT INTO vcs_intents(intent_id,project_id,idempotency_key,request_json,state,receipt_json,created_at,updated_at) VALUES(?1,?2,?3,?4,'accepted',?5,?6,?6)", params![id,owner.project_id,request.idempotency_key,body,json(&receipt)?,now_ms])?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn attach_vcs_operation(
        &mut self,
        owner: &OwnerFence,
        store: &LocalStore,
        intent_id: &str,
        prepared: &PreparedVcsOperation,
        now_ms: i64,
    ) -> Result<VcsIntentReceipt> {
        prepared.validate()?;
        let existing = intent(&self.conn, &owner.project_id, intent_id)?;
        if let Some(old) = &existing.prepared {
            ensure!(
                old == prepared,
                "VCS intent already has different prepared operation"
            );
            return Ok(existing);
        }
        ensure!(
            existing.state == VcsIntentState::Accepted,
            "VCS intent is not awaiting preparation"
        );
        validate_recovery(store, &existing.request)?;
        validate_record(store, &existing, prepared)?;
        let pins = retain(
            store,
            intent_id,
            vec![prepared.record.clone(), prepared.proof_identity.clone()],
        )?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut receipt = intent(&tx, &owner.project_id, intent_id)?;
        if let Some(old) = &receipt.prepared {
            ensure!(
                old == prepared,
                "VCS intent already has different prepared operation"
            );
            return Ok(receipt);
        }
        ensure!(
            receipt.state == VcsIntentState::Accepted,
            "VCS intent is not awaiting preparation"
        );
        ensure!(
            receipt.original_owner == *owner,
            "intent belongs to another owner epoch"
        );
        super::decisions::validate_integration_gate(
            &tx,
            owner,
            &receipt.request.integration(&prepared.commit_id),
            None,
        )?;
        source_fence(&tx, &owner.project_id, &receipt.request.source_run)?;
        receipt.retention_pins.extend(pins);
        receipt.prepared = Some(prepared.clone());
        receipt.state = VcsIntentState::Prepared;
        save(
            &tx,
            &mut receipt,
            &owner.owner_id,
            "vcs-operation-prepared",
            now_ms,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn vcs_intent(&self, owner: &OwnerFence, intent_id: &str) -> Result<VcsIntentReceipt> {
        // Trusted local history accessor; no agent-facing raw journal access.
        intent(&self.conn, &owner.project_id, intent_id)
    }

    pub fn publish_vcs_intent(
        &mut self,
        owner: &OwnerFence,
        intent_id: &str,
        publisher: &mut dyn VcsPublisher,
        now_ms: i64,
    ) -> Result<VcsIntentReceipt> {
        {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut receipt = intent(&tx, &owner.project_id, intent_id)?;
            if matches!(
                receipt.state,
                VcsIntentState::Published | VcsIntentState::Rejected
            ) {
                return Ok(receipt);
            }
            ensure!(
                receipt.state == VcsIntentState::Prepared,
                "VCS effect cannot be replayed; reconcile started/uncertain intent"
            );
            ensure!(
                receipt.original_owner == *owner,
                "intent belongs to another owner epoch"
            );
            let prepared = receipt
                .prepared
                .as_ref()
                .context("prepared operation missing")?;
            super::decisions::validate_integration_gate(
                &tx,
                owner,
                &receipt.request.integration(&prepared.commit_id),
                None,
            )?;
            source_fence(&tx, &owner.project_id, &receipt.request.source_run)?;
            receipt.state = VcsIntentState::Started;
            save(
                &tx,
                &mut receipt,
                &owner.owner_id,
                "vcs-publication-started",
                now_ms,
            )?;
            tx.commit()?;
        }
        // A crash from here leaves durable Started. It is never interpreted as
        // no effect. This transaction fences decision acceptance and competing
        // integration until physical publication and its receipt both complete.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut receipt = intent(&tx, &owner.project_id, intent_id)?;
        ensure!(
            receipt.state == VcsIntentState::Started && receipt.original_owner == *owner,
            "publication start changed"
        );
        let prepared = receipt
            .prepared
            .clone()
            .context("prepared operation missing")?;
        let request = receipt.request.integration(&prepared.commit_id);
        super::decisions::validate_integration_gate(&tx, owner, &request, Some(intent_id))?;
        source_fence(&tx, &owner.project_id, &receipt.request.source_run)?;
        let outcome = match publisher.inspect(&prepared) {
            Ok(VcsPublicationObservation::NotPublished) => publisher.publish(&prepared),
            Ok(other) => Ok(VcsPublicationObservation::Uncertain {
                message: format!("unexpected pre-publication state: {other:?}"),
            }),
            Err(error) => Ok(VcsPublicationObservation::Uncertain {
                message: format!("publication preflight failed after durable start: {error:#}"),
            }),
        };
        match outcome {
            Ok(VcsPublicationObservation::Published) => {
                receipt.integration = Some(super::decisions::apply_integration(
                    &tx, owner, &request, now_ms,
                )?);
                receipt.state = VcsIntentState::Published;
                receipt.result = Some(
                    serde_json::json!({"observed":"published","historical_reconciliation":false}),
                );
            }
            Ok(other) => make_uncertain(
                &mut receipt,
                &format!("external publication requires reconciliation: {other:?}"),
            ),
            Err(error) => make_uncertain(
                &mut receipt,
                &format!("external publication outcome is unknown: {error:#}"),
            ),
        }
        save(
            &tx,
            &mut receipt,
            &owner.owner_id,
            "vcs-publication-outcome",
            now_ms,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Inspect only. Never reissue publish, merge, restore, or rebase here.
    pub fn reconcile_vcs_intent(
        &mut self,
        owner: &OwnerFence,
        intent_id: &str,
        publisher: &mut dyn VcsPublisher,
        now_ms: i64,
    ) -> Result<VcsIntentReceipt> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut receipt = intent(&tx, &owner.project_id, intent_id)?;
        if matches!(
            receipt.state,
            VcsIntentState::Published | VcsIntentState::Rejected
        ) {
            return Ok(receipt);
        }
        assert_owner(&tx, owner)?;
        ensure!(
            matches!(
                receipt.state,
                VcsIntentState::Started | VcsIntentState::Uncertain
            ),
            "only started/uncertain publication can be reconciled"
        );
        let prepared = receipt
            .prepared
            .clone()
            .context("prepared operation missing")?;
        let request = receipt.request.integration(&prepared.commit_id);
        match publisher.inspect(&prepared) {
            Ok(VcsPublicationObservation::Published) => {
                // This observes an old immutable effect, not a new action by a
                // retired source/owner. The current owner and current head/full
                // decision vector still gate adoption of that historical fact.
                match super::decisions::validate_integration_gate(
                    &tx,
                    owner,
                    &request,
                    Some(intent_id),
                ) {
                    Ok(()) => {
                        let mut historical_owner = owner.clone();
                        historical_owner.epoch = receipt.original_owner.epoch;
                        historical_owner.owner_id = receipt.original_owner.owner_id.clone();
                        receipt.integration = Some(super::decisions::apply_integration(
                            &tx,
                            &historical_owner,
                            &request,
                            now_ms,
                        )?);
                        receipt.state = VcsIntentState::Published;
                        receipt.result = Some(
                            serde_json::json!({"observed":"published","historical_reconciliation":true,"reconciled_by":owner}),
                        );
                    }
                    Err(error) => {
                        make_uncertain(
                            &mut receipt,
                            &format!("published operation has changed adoption fence: {error:#}"),
                        );
                        receipt.result.as_mut().unwrap()["observed"] =
                            Value::String("published".into());
                    }
                }
            }
            Ok(VcsPublicationObservation::NotPublished) => {
                receipt.state = VcsIntentState::Rejected;
                receipt.result = Some(
                    serde_json::json!({"observed":"not_published","reason":"exact private-repository head set unchanged; original operation will not be replayed"}),
                );
            }
            Ok(other) => make_uncertain(
                &mut receipt,
                &format!("cannot reconcile changed/unknown history: {other:?}"),
            ),
            Err(error) => make_uncertain(
                &mut receipt,
                &format!("cannot inspect exact prepared operation: {error:#}"),
            ),
        }
        save(
            &tx,
            &mut receipt,
            &owner.owner_id,
            "vcs-publication-reconciled",
            now_ms,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn recover_vcs_intents(
        &mut self,
        owner: &OwnerFence,
        now_ms: i64,
    ) -> Result<Vec<VcsIntentReceipt>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_owner(&tx, owner)?;
        let ids = {
            let mut query = tx.prepare("SELECT intent_id FROM vcs_intents WHERE project_id=?1 AND state='started' ORDER BY intent_id")?;
            let ids = query
                .query_map([&owner.project_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        let mut receipts = Vec::new();
        for id in ids {
            let mut receipt = intent(&tx, &owner.project_id, &id)?;
            make_uncertain(
                &mut receipt,
                "engine stopped after durable publication start; inspect the original operation",
            );
            save(
                &tx,
                &mut receipt,
                &owner.owner_id,
                "vcs-publication-recovered",
                now_ms,
            )?;
            receipts.push(receipt);
        }
        tx.commit()?;
        Ok(receipts)
    }
}

pub(super) fn assert_no_uncertain_integration(
    conn: &Connection,
    project: &str,
    except: Option<&str>,
) -> Result<()> {
    let count: i64 = conn.query_row("SELECT count(*) FROM vcs_intents WHERE project_id=?1 AND state IN('started','uncertain') AND (?2 IS NULL OR intent_id!=?2)", params![project,except], |r| r.get(0))?;
    ensure!(
        count == 0,
        "unreconciled VCS publication blocks integration"
    );
    Ok(())
}

pub(super) fn assert_legacy_key_unreserved(
    conn: &Connection,
    project: &str,
    key: &str,
) -> Result<()> {
    let used: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM vcs_intents WHERE project_id=?1 AND idempotency_key=?2)",
        params![project, key],
        |r| r.get(0),
    )?;
    ensure!(!used, "idempotency key belongs to a durable VCS intent");
    Ok(())
}
fn assert_integration_key_available(conn: &Connection, project: &str, key: &str) -> Result<()> {
    let used: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM integrations WHERE project_id=?1 AND idempotency_key=?2)",
        params![project, key],
        |r| r.get(0),
    )?;
    ensure!(!used, "idempotency key belongs to a completed integration");
    Ok(())
}
fn source_fence(conn: &Connection, project: &str, source: &VcsRunFence) -> Result<()> {
    let (epoch, attempt): (u64, Option<String>) = conn.query_row(
        "SELECT epoch,instance_id FROM runs WHERE project_id=?1 AND run_id=?2",
        params![project, source.run_id],
        |r| Ok((db_u64(r, 0)?, r.get(1)?)),
    )?;
    ensure!(
        epoch == source.run_epoch && attempt.as_deref() == Some(source.attempt_id.as_str()),
        "source run epoch or attempt changed"
    );
    Ok(())
}
fn duplicate(
    conn: &Connection,
    project: &str,
    key: &str,
    body: &str,
) -> Result<Option<VcsIntentReceipt>> {
    let row: Option<(String, String)> = conn.query_row("SELECT request_json,receipt_json FROM vcs_intents WHERE project_id=?1 AND idempotency_key=?2", params![project,key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((old, receipt)) = row {
        ensure!(
            old == body,
            "idempotency key reused with a different VCS intent"
        );
        return Ok(Some(serde_json::from_str(&receipt)?));
    }
    Ok(None)
}
fn intent(conn: &Connection, project: &str, id: &str) -> Result<VcsIntentReceipt> {
    valid_id(id)?;
    let body: String = conn.query_row(
        "SELECT receipt_json FROM vcs_intents WHERE project_id=?1 AND intent_id=?2",
        params![project, id],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&body)?)
}
fn save(
    tx: &Transaction<'_>,
    receipt: &mut VcsIntentReceipt,
    principal: &str,
    event: &str,
    now_ms: i64,
) -> Result<()> {
    receipt.sequence = append_event(
        tx,
        &receipt.project_id,
        event,
        principal,
        &serde_json::json!({"intent_id":receipt.intent_id,"state":receipt.state,"result":receipt.result}),
        now_ms,
    )?;
    let state = serde_json::to_value(receipt.state)?
        .as_str()
        .unwrap()
        .to_string();
    tx.execute("UPDATE vcs_intents SET state=?1,receipt_json=?2,updated_at=?3 WHERE project_id=?4 AND intent_id=?5", params![state,json(receipt)?,now_ms,receipt.project_id,receipt.intent_id])?;
    Ok(())
}
fn make_uncertain(receipt: &mut VcsIntentReceipt, reason: &str) {
    receipt.state = VcsIntentState::Uncertain;
    receipt.result = Some(serde_json::json!({"reason":reason}));
}
fn retain(
    store: &LocalStore,
    id: &str,
    roots: Vec<ObjectRef>,
) -> Result<BTreeMap<String, ObjectRef>> {
    let mut pins = BTreeMap::new();
    for root in roots {
        let pin = format!("vcs-intent/{id}/{:?}/{}", root.kind, root.digest);
        if store.retention()?.pins.values().any(|old| old == &root) {
            store.pin_existing(&pin, &root)?;
        } else {
            store.pin(&pin, &root)?;
        }
        pins.insert(pin, root);
    }
    Ok(pins)
}
fn validate_record(
    store: &LocalStore,
    intent: &VcsIntentReceipt,
    prepared: &PreparedVcsOperation,
) -> Result<()> {
    let bytes = store.read_object(&prepared.record)?;
    let record: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        canonical_json(&record)? == bytes,
        "prepared bridge record is not canonical JSON"
    );
    ensure!(
        matches!(record["format_version"].as_u64(), Some(1 | 2))
            && record["intent_id"] == intent.intent_id
            && record["operation"] == prepared.operation_id
            && record["expected_heads"]
                == serde_json::to_value(&prepared.expected_operation_heads)?
            && record["commit"]["commit"] == prepared.commit_id
            && record["integration_base"] == intent.request.expected_head,
        "prepared bridge operation identity differs"
    );
    match record["format_version"].as_u64() {
        Some(1) => ensure!(
            record.get("projection_receipt").is_none(),
            "legacy prepared record cannot carry projection evidence"
        ),
        Some(2) => {
            let projection: ObjectRef =
                serde_json::from_value(record["projection_receipt"].clone())?;
            projection.validate()?;
            ensure!(
                projection.kind == ObjectKind::ActionResult,
                "source projection receipt must be an ActionResult"
            );
            ensure!(
                intent.request.evidence.action_results.contains(&projection),
                "source projection receipt was not retained in accepted intent evidence"
            );
        }
        _ => bail!("unsupported prepared bridge record"),
    }
    let proof = &record["proof"];
    prepared.proof_identity.verify(&canonical_json(proof)?)?;
    ensure!(
        store.read_object(&prepared.proof_identity)? == canonical_json(proof)?,
        "prepared proof bytes differ"
    );
    let binding = &proof["binding"];
    ensure!(
        proof["format_version"] == 1
            && proof["source"] == serde_json::to_value(&intent.request.proposed_source_root)?
            && proof["environment"]
                == serde_json::to_value(&intent.request.proposed_environment_root)?
            && proof["recovery_root"] == serde_json::to_value(&intent.request.recovery_root)?
            && proof["tree"] == record["commit"]["tree"],
        "prepared source/recovery/environment/tree binding differs"
    );
    ensure!(
        binding["project_id"] == intent.project_id
            && binding["owner"]["owner"] == intent.original_owner.owner_id
            && binding["owner"]["epoch"] == intent.original_owner.epoch
            && binding["run_id"] == intent.request.source_run.run_id
            && binding["run_epoch"] == intent.request.source_run.run_epoch
            && binding["attempt_id"] == intent.request.source_run.attempt_id
            && binding["view_id"] == intent.request.source_run.view_id
            && binding["seed"] == serde_json::to_value(&intent.request.seed_root)?
            && binding["projection_policy"]
                == serde_json::to_value(&intent.request.projection_policy)?
            && binding["profile"] == serde_json::to_value(&intent.request.profile)?,
        "prepared attempt/authority/seed/projection binding differs"
    );
    Ok(())
}

fn validate_recovery(store: &LocalStore, request: &VcsIntentRequest) -> Result<()> {
    ensure!(
        request.recovery_root.size_bytes <= 1024 * 1024,
        "recovery manifest exceeds size limit"
    );
    let bytes = store.read_object(&request.recovery_root)?;
    let recovery: Value = serde_json::from_slice(&bytes)?;
    // First slice supports exact versioned-source seals. A different subset
    // needs a trusted derivation receipt; JSON refs alone cannot prove it.
    ensure!(
        recovery["source"] == serde_json::to_value(&request.proposed_source_root)?
            && recovery["environment"] == serde_json::to_value(&request.proposed_environment_root)?,
        "stored recovery source/environment differs from intent"
    );
    let source_bytes = store.read_object(&request.proposed_source_root)?;
    let source: Value = serde_json::from_slice(&source_bytes)?;
    if source["format_version"] == 2 {
        let source = crate::snapshot::decode_source(&source_bytes)?;
        ensure!(
            source.projection_policy == request.projection_policy,
            "stored source projection policy differs"
        );
    }
    match request.recovery_root.kind {
        ObjectKind::RunnableRoot => {
            let runnable = crate::snapshot::decode_runnable(&bytes)?;
            ensure!(
                Some(runnable.view_root) == request.expected_view_root
                    && runnable.profile == request.profile
                    && Some(runnable.hardlinks) == request.hardlinks,
                "stored runnable view/profile/hardlinks differ"
            );
            crate::snapshot::decode_source(&source_bytes)?;
            crate::snapshot::decode_environment(
                &store.read_object(&request.proposed_environment_root)?,
            )?;
        }
        ObjectKind::Checkpoint => match recovery["format_version"].as_u64() {
            Some(1) => {
                let _: crate::store::CheckpointManifest = serde_json::from_slice(&bytes)?;
                ensure!(
                    request.expected_view_root.is_none() && request.hardlinks.is_none(),
                    "legacy checkpoint does not prove a complete view/topology binding"
                );
            }
            Some(2) => {
                let checkpoint: crate::store::CheckpointV2Manifest =
                    serde_json::from_slice(&bytes)?;
                ensure!(
                    Some(checkpoint.view_root) == request.expected_view_root
                        && checkpoint.profile == request.profile
                        && request.hardlinks.is_none(),
                    "stored checkpoint view/profile differs or lacks topology"
                );
            }
            Some(3) => {
                let checkpoint: crate::store::CheckpointV3Manifest =
                    serde_json::from_slice(&bytes)?;
                ensure!(
                    Some(checkpoint.view_root) == request.expected_view_root
                        && checkpoint.profile == request.profile
                        && Some(checkpoint.hardlinks) == request.hardlinks,
                    "stored checkpoint view/profile/hardlinks differ"
                );
            }
            _ => bail!("unsupported recovery checkpoint version"),
        },
        _ => bail!("invalid VCS recovery root"),
    }
    Ok(())
}
fn kind(reference: &ObjectRef, expected: ObjectKind) -> Result<()> {
    reference.validate()?;
    ensure!(reference.kind == expected, "incorrect VCS reference kind");
    Ok(())
}
fn raw_hex(value: &str, length: usize) -> Result<()> {
    ensure!(
        value.len() == length
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid raw VCS identity"
    );
    Ok(())
}
