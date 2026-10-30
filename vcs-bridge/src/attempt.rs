//! Fresh per-attempt jj history identities without scanning source files.

use crate::{atomic, seed::*, working_copy::*, *};
use anyhow::{Context, Result, ensure};
use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{RepoLoader, StoreFactories};
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::WorkingCopyFactory;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptHistoryReceipt {
    pub format_version: u32,
    pub binding: AttemptBinding,
    pub working_copy: CommitIdentity,
    pub operation: JjOperationId,
}

/// Initialize or reconcile the SAME private attempt. It never changes canonical
/// history, copies source, adds a shared jj workspace, or invokes LocalWC.
pub async fn initialize_attempt_history(
    metadata: &Path,
    binding: &AttemptBinding,
    settings: &UserSettings,
    view: Arc<dyn WorkspaceView>,
) -> Result<AttemptHistoryReceipt> {
    binding.validate()?;
    ensure!(
        std::fs::read_to_string(metadata.join(".jj/working_copy/type"))? == TYPE_NAME,
        "metadata-only attempt initialization requires the Rudder VFS working copy"
    );
    let _lock = atomic::lock(&metadata.join("rudder-attempt.lock"))?;
    view.validate_authority(binding)?;
    let loader = RepoLoader::init_from_file_system(
        settings,
        &metadata.join(".jj/repo"),
        &StoreFactories::default(),
    )?;
    let heads = loader.op_heads_store().get_op_heads().await?;
    ensure!(
        heads.len() == 1,
        "private attempt has unexpected divergent operation heads"
    );
    let operation = loader.load_operation(&heads[0]).await?;
    let repo = loader.load_at(&operation).await?;
    let digest = rudder_engine::protocol::ObjectRef::from_bytes(
        rudder_engine::protocol::ObjectKind::Blob,
        &rudder_engine::protocol::canonical_json(binding)?,
    )
    .digest;
    let initialized = operation.metadata().tags.get("rudder.attempt.binding") == Some(&digest);
    let commit = if initialized {
        let id = repo
            .view()
            .get_wc_commit_id(WorkspaceName::DEFAULT)
            .context("private attempt working-copy commit missing")?;
        let commit = loader.store().get_commit(id)?;
        ensure!(
            commit.parent_ids() == [CommitId::new(hex::decode(&binding.baseline.commit.0)?)]
                && JjTreeIdentity::from_jj(&commit.tree()) == binding.baseline.tree,
            "prepared private attempt differs from its exact baseline"
        );
        ensure!(
            CommitIdentity::from_jj(&commit).change != binding.baseline.change,
            "attempt inherited the baseline change identity"
        );
        commit
    } else {
        ensure!(
            JjOperationId(operation.id().hex()) == binding.baseline_operation,
            "private history changed; do not replay attempt initialization"
        );
        let baseline = loader
            .store()
            .get_commit(&CommitId::new(hex::decode(&binding.baseline.commit.0)?))?;
        ensure!(
            CommitIdentity::from_jj(&baseline) == binding.baseline,
            "baseline commit/change/tree identity differs"
        );
        let mut tx = repo.start_transaction();
        tx.set_tag("rudder.attempt.binding".into(), digest);
        let commit = tx
            .repo_mut()
            .new_commit(vec![baseline.id().clone()], baseline.tree())
            .set_change_id(ChangeId::new(uuid::Uuid::new_v4().as_bytes().to_vec()))
            .set_description(format!("rudder attempt {}", binding.attempt_id))
            .write()
            .await?;
        // Do not abandon or rewrite any inherited canonical ancestor.
        tx.repo_mut()
            .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), commit.id().clone())?;
        tx.commit(format!(
            "initialize private Rudder attempt {}",
            binding.attempt_id
        ))
        .await?;
        commit
    };
    let current_heads = loader.op_heads_store().get_op_heads().await?;
    ensure!(
        current_heads.len() == 1,
        "private operation heads changed during initialization"
    );
    let factory = RudderWorkingCopyFactory::new(view, None);
    let wc = factory.load_working_copy(
        loader.store().clone(),
        metadata.to_path_buf(),
        metadata.join(".jj/working_copy"),
        settings,
    )?;
    let mut locked = wc.start_mutation()?;
    locked.reset(&commit).await?;
    locked.finish(current_heads[0].clone()).await?;
    let receipt = AttemptHistoryReceipt {
        format_version: 1,
        binding: binding.clone(),
        working_copy: CommitIdentity::from_jj(&commit),
        operation: JjOperationId(current_heads[0].hex()),
    };
    atomic::write_json(&metadata.join("rudder-attempt-ready.json"), &receipt)?;
    Ok(receipt)
}

/// Production activation requires both private metadata and a fresh WC change
/// identity. An interrupted call must reconcile its existing private operation;
/// it must not overwrite the directory or mint another attempt.
pub async fn fork_private_attempt(
    template: &VerifiedMetadataTemplate,
    destination: &Path,
    binding: AttemptBinding,
    observation: &LiveObservation,
    settings: &UserSettings,
    view: Arc<dyn WorkspaceView>,
) -> Result<AttemptHistoryReceipt> {
    ensure!(
        template.retained_seed() == Some(&binding.seed),
        "production attempt requires the exact retained VCS artifact"
    );
    fork_private_metadata(template, destination, binding.clone(), Some(observation))?;
    initialize_attempt_history(destination, &binding, settings, view).await
}
