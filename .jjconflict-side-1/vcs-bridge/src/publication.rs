//! Prepare immutable jj operations separately from visible history publication.
//!
//! The caller holds the engine's private integration-repository lock and final
//! journal owner/run/head/decision gate across publish. jj's op-head lock alone
//! cannot fence old clients or atomically commit the SQLite authority journal.

use crate::*;
use anyhow::{Result, ensure};
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::OperationId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefName, WorkspaceName};
use jj_lib::repo::{Repo, RepoLoader};
use jj_lib::transaction::Transaction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedJjOperation {
    pub format_version: u32,
    pub intent_id: String,
    pub expected_heads: Vec<JjOperationId>,
    pub operation: JjOperationId,
    pub integration_base: GitCommitId,
    pub commit: CommitIdentity,
    pub proof: SealedSourceProof,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_receipt: Option<rudder_engine::protocol::ObjectRef>,
}
impl PreparedJjOperation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.format_version, 1 | 2),
            "unsupported prepared operation version"
        );
        match (self.format_version, &self.projection_receipt) {
            (1, None) => {}
            (2, Some(reference)) => {
                reference.validate()?;
                ensure!(
                    reference.kind == rudder_engine::protocol::ObjectKind::ActionResult,
                    "projection receipt must be an ActionResult"
                );
            }
            _ => anyhow::bail!("prepared operation version/projection receipt mismatch"),
        }
        crate::identity::valid_name(&self.intent_id)?;
        self.operation.validate()?;
        self.integration_base.validate()?;
        validate_heads(&self.expected_heads)?;
        ensure!(
            !self.expected_heads.contains(&self.operation),
            "operation cannot replace itself"
        );
        self.commit.validate()?;
        self.proof.validate()?;
        ensure!(
            self.commit.tree == self.proof.tree,
            "sealed projection differs from proposed commit tree"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PublicationState {
    NotPublished,
    Published,
    /// Never auto-rebase, merge operation heads, or silently publish here.
    HeadChanged {
        actual_heads: Vec<JjOperationId>,
    },
    /// A visible update was attempted and did not return a definite outcome.
    /// The caller persists uncertainty and inspects the same intent next time.
    Uncertain {
        message: String,
    },
}

/// Caller has already durably accepted intent_id. This method only writes jj
/// objects/index/op data. It never changes the op-head set or merges at head.
pub async fn prepare_operation(
    transaction: Transaction,
    intent_id: String,
    expected_heads: Vec<JjOperationId>,
    integration_base: GitCommitId,
    commit: CommitIdentity,
    proof: SealedSourceProof,
) -> Result<PreparedJjOperation> {
    prepare_operation_inner(
        transaction,
        intent_id,
        expected_heads,
        integration_base,
        commit,
        proof,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn prepare_operation_inner(
    mut transaction: Transaction,
    intent_id: String,
    expected_heads: Vec<JjOperationId>,
    integration_base: GitCommitId,
    commit: CommitIdentity,
    proof: SealedSourceProof,
    projection_receipt: Option<rudder_engine::protocol::ObjectRef>,
) -> Result<PreparedJjOperation> {
    crate::identity::valid_name(&intent_id)?;
    validate_heads(&expected_heads)?;
    commit.validate()?;
    integration_base.validate()?;
    proof.validate()?;
    ensure!(
        commit.tree == proof.tree,
        "sealed projection differs from proposed commit tree"
    );
    let actual = transaction
        .base_repo()
        .store()
        .get_commit(&CommitId::new(hex::decode(&commit.commit.0)?))?;
    ensure!(
        CommitIdentity::from_jj(&actual) == commit,
        "commit identity differs from jj object"
    );
    let base = CommitId::new(hex::decode(&integration_base.0)?);
    ensure!(
        integration_head(transaction.base_repo().view()) == Some(&base),
        "private integration head differs from expected base"
    );
    ensure!(
        transaction.repo().index().is_ancestor(&base, actual.id())?,
        "proposed history does not preserve the integration base ancestry"
    );
    transaction.repo_mut().set_local_bookmark_target(
        RefName::new(INTEGRATION_BOOKMARK),
        RefTarget::normal(actual.id().clone()),
    );
    transaction.set_tag("rudder.intent".into(), intent_id.clone());
    transaction.set_tag("rudder.proof".into(), proof.identity()?.digest);
    transaction.set_tag("rudder.commit".into(), commit.commit.0.clone());
    transaction.set_tag("rudder.base".into(), integration_base.0.clone());
    if let Some(reference) = &projection_receipt {
        transaction.set_tag("rudder.projection".into(), reference.digest.clone());
    }
    let unpublished = transaction
        .write(format!("rudder integration {intent_id}"))
        .await?;
    let operation = unpublished.operation();
    let parents = sorted_heads(
        operation
            .parent_ids()
            .iter()
            .map(|id| JjOperationId(id.hex()))
            .collect(),
    );
    let prepared = PreparedJjOperation {
        format_version: if projection_receipt.is_some() { 2 } else { 1 },
        intent_id,
        expected_heads,
        operation: JjOperationId(operation.id().hex()),
        integration_base,
        commit,
        proof,
        projection_receipt,
    };
    // Explicitly leave unpublished even on a parent mismatch; no Drop side
    // effect should ever advance a visible history head.
    unpublished.leave_unpublished();
    ensure!(
        parents == sorted_heads(prepared.expected_heads.clone()),
        "transaction parents differ from expected complete op-head set"
    );
    prepared.validate()?;
    Ok(prepared)
}

/// The native caller should use this after a trusted terminal writer barrier
/// and `verify_versioned_source`, then retain the mapping receipt as evidence.
/// Filesystem quiescence and current owner/decision gates remain engine-owned.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_verified_operation(
    transaction: Transaction,
    intent_id: String,
    expected_heads: Vec<JjOperationId>,
    integration_base: GitCommitId,
    commit: CommitIdentity,
    proof: SealedSourceProof,
    verified: &crate::projection::VerifiedSourceProjection,
) -> Result<PreparedJjOperation> {
    verified.check_proof(&proof)?;
    prepare_operation_inner(
        transaction,
        intent_id,
        expected_heads,
        integration_base,
        commit,
        proof,
        Some(verified.evidence().clone()),
    )
    .await
}

async fn validate_stored(loader: &RepoLoader, prepared: &PreparedJjOperation) -> Result<()> {
    prepared.validate()?;
    let op = loader
        .load_operation(&OperationId::new(hex::decode(&prepared.operation.0)?))
        .await?;
    let tags = &op.metadata().tags;
    match &prepared.projection_receipt {
        Some(reference) => ensure!(
            tags.get("rudder.projection") == Some(&reference.digest),
            "stored operation projection receipt differs"
        ),
        None => ensure!(
            !tags.contains_key("rudder.projection"),
            "legacy operation cannot carry an unbound projection receipt"
        ),
    }
    ensure!(
        tags.get("rudder.intent") == Some(&prepared.intent_id),
        "stored operation belongs to another intent"
    );
    ensure!(
        tags.get("rudder.proof") == Some(&prepared.proof.identity()?.digest),
        "stored operation proof differs"
    );
    ensure!(
        tags.get("rudder.commit") == Some(&prepared.commit.commit.0),
        "stored operation commit differs"
    );
    ensure!(
        tags.get("rudder.base") == Some(&prepared.integration_base.0),
        "stored integration base differs"
    );
    ensure!(
        sorted_heads(
            op.parent_ids()
                .iter()
                .map(|id| JjOperationId(id.hex()))
                .collect()
        ) == sorted_heads(prepared.expected_heads.clone()),
        "stored operation parent set differs"
    );
    let commit = loader
        .store()
        .get_commit(&CommitId::new(hex::decode(&prepared.commit.commit.0)?))?;
    ensure!(
        CommitIdentity::from_jj(&commit) == prepared.commit,
        "stored commit/change/tree identity differs"
    );
    // Loading at this exact operation must work without resolving current heads.
    let repo = loader.load_at(&op).await?;
    ensure!(
        repo.view()
            .get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK))
            .as_normal()
            == Some(commit.id()),
        "prepared view does not publish the exact target commit"
    );
    let base = CommitId::new(hex::decode(&prepared.integration_base.0)?);
    ensure!(
        repo.index().is_ancestor(&base, commit.id())?,
        "prepared history loses canonical base ancestry"
    );
    for parent in op.parents().await? {
        ensure!(
            integration_head(&parent.view().await?) == Some(&base),
            "previous private integration view differs from expected base"
        );
    }
    Ok(())
}

/// Exact inspection does not call load_at_head(), which could merge divergent
/// operations as a side effect. Historical descendant reconciliation remains a
/// caller decision; changed heads are returned honestly instead of replayed.
pub async fn inspect_operation(
    loader: &RepoLoader,
    prepared: &PreparedJjOperation,
) -> Result<PublicationState> {
    validate_stored(loader, prepared).await?;
    Ok(classify(loader, prepared).await?)
}

async fn classify(loader: &RepoLoader, prepared: &PreparedJjOperation) -> Result<PublicationState> {
    let heads = sorted_heads(
        loader
            .op_heads_store()
            .get_op_heads()
            .await?
            .iter()
            .map(|id| JjOperationId(id.hex()))
            .collect(),
    );
    if heads == [prepared.operation.clone()] {
        Ok(PublicationState::Published)
    } else if heads == sorted_heads(prepared.expected_heads.clone()) {
        Ok(PublicationState::NotPublished)
    } else {
        Ok(PublicationState::HeadChanged {
            actual_heads: heads,
        })
    }
}

/// Idempotently publish this exact operation. Call only after the engine's
/// final fence succeeds and while that gate remains held. Errors before the
/// external write are definite errors; a failed write becomes Uncertain.
pub async fn publish_operation(
    loader: &RepoLoader,
    prepared: &PreparedJjOperation,
) -> Result<PublicationState> {
    validate_stored(loader, prepared).await?;
    let _op_lock = loader.op_heads_store().lock().await?;
    let before = classify(loader, prepared).await?;
    if before != PublicationState::NotPublished {
        return Ok(before);
    }
    let previous = prepared
        .expected_heads
        .iter()
        .map(|id| hex::decode(&id.0).map(OperationId::new))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let next = OperationId::new(hex::decode(&prepared.operation.0)?);
    if let Err(error) = loader
        .op_heads_store()
        .update_op_heads(&previous, &next)
        .await
    {
        return Ok(PublicationState::Uncertain {
            message: format!("jj publication outcome requires reconciliation: {error}"),
        });
    }
    match classify(loader, prepared).await {
        Ok(PublicationState::Published) => Ok(PublicationState::Published),
        Ok(other) => Ok(PublicationState::Uncertain {
            message: format!("op heads changed during publication: {other:?}"),
        }),
        Err(error) => Ok(PublicationState::Uncertain {
            message: format!("publication inspection failed: {error}"),
        }),
    }
}

pub async fn reconcile_operation(
    loader: &RepoLoader,
    prepared: &PreparedJjOperation,
) -> Result<PublicationState> {
    inspect_operation(loader, prepared).await
}

fn validate_heads(heads: &[JjOperationId]) -> Result<()> {
    ensure!(
        !heads.is_empty() && heads.len() <= 1024,
        "invalid expected op-head count"
    );
    for head in heads {
        head.validate()?;
    }
    let set: std::collections::BTreeSet<_> = heads.iter().collect();
    ensure!(set.len() == heads.len(), "duplicate expected op head");
    Ok(())
}
fn sorted_heads(mut heads: Vec<JjOperationId>) -> Vec<JjOperationId> {
    heads.sort();
    heads
}

pub const INTEGRATION_BOOKMARK: &str = "rudder-integration";
fn integration_head(view: &jj_lib::view::View) -> Option<&CommitId> {
    let target = view.get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK));
    if target.is_absent() {
        view.get_wc_commit_id(WorkspaceName::DEFAULT)
    } else {
        target.as_normal()
    }
}
