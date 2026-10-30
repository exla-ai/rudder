//! Narrow trusted host composition. Native callers need the `jj` feature but
//! never depend directly on jj-cli or construct upstream settings themselves.
use crate::{
    atomic, attempt::*, command_transport::*, rpc_view::*, vfs_provider, working_copy::*, *,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{Repo, RepoLoader, StoreFactories};
use jj_lib::working_copy::WorkingCopyFactory;
use rudder_engine::store::LocalStore;
use rudder_engine::vfs::rpc::AdoptionWitness;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

pub fn fence_private_workspace(
    config: &CommandServiceConfig,
    timeout: Duration,
) -> Result<CommandFinalizationGuard> {
    config.validate()?;
    fence_private_writer(config, timeout)
}

pub fn working_copy_witness(config: &CommandServiceConfig) -> Result<Arc<dyn AdoptionWitness>> {
    config.validate()?;
    Ok(Arc::new(DurableWorkingCopyWitness::new(
        config.view.clone(),
        settings(config)?,
    )?))
}

/// Bind the exact existing private prepared baseline; never initialize through
/// LocalWorkingCopy, copy source, or adopt an unrelated/current operation head.
/// Lost initialization readiness can reconcile its already tagged operation.
/// A previously ready same attempt is inspected rather than reset/replayed.
pub fn prepare_workspace(
    config: &CommandServiceConfig,
    authority: Arc<dyn vfs_provider::AttemptAuthority>,
) -> Result<AttemptHistoryReceipt> {
    config.check_prepared_configuration()?;
    let _preparation = lock_file(
        &config
            .view
            .private_repository
            .join("rudder-host-preparation.lock"),
    )?;
    _preparation
        .try_lock_exclusive()
        .context("private VCS preparation is already owned")?;
    let writer = lock_file(&config.view.private_repository.join("rudder-command.lock"))?;
    writer
        .try_lock_exclusive()
        .context("a private VCS writer is still live")?;
    let view = Arc::new(RpcWorkspaceView::new(
        config.view.clone(),
        LocalStore::open(&config.view.cas_root)?,
        authority,
    )?);
    view.validate_authority(&config.view.attempt)?;
    let settings = settings(config)?;
    pollster::block_on(async {
        let loader = RepoLoader::init_from_file_system(
            &settings,
            &config.view.private_repository.join(".jj/repo"),
            &StoreFactories::default(),
        )?;
        ensure!(
            crate::projection_io::git_backend(loader.store())?
                .git_repo_path()
                .canonicalize()?
                == config.view.private_git.canonicalize()?,
            "private jj metadata points outside its configured Git store"
        );
        let heads = loader.op_heads_store().get_op_heads().await?;
        ensure!(
            heads.len() == 1,
            "private operation heads diverged; no automatic merge"
        );
        let operation = loader.load_operation(&heads[0]).await?;
        let repo = loader.load_at(&operation).await?;
        let baseline = repo.store().get_commit(&CommitId::new(hex::decode(
            &config.view.attempt.baseline.commit.0,
        )?))?;
        ensure!(
            CommitIdentity::from_jj(&baseline) == config.view.attempt.baseline,
            "prepared baseline identity changed"
        );
        let ready = config
            .view
            .private_repository
            .join("rudder-attempt-ready.json");
        if ready.try_exists()? {
            ensure!(
                fs::read_to_string(config.view.working_copy_state().join("type"))? == TYPE_NAME,
                "ready attempt lost its Rudder working-copy type"
            );
            let receipt: AttemptHistoryReceipt = atomic::read_json(&ready)?;
            ensure!(
                receipt.format_version == 1
                    && receipt.binding == config.view.attempt
                    && receipt.working_copy.change != config.view.attempt.baseline.change,
                "private readiness belongs to another attempt"
            );
            receipt.operation.validate()?;
            receipt.working_copy.validate()?;
            let initial = loader
                .load_operation(&jj_lib::op_store::OperationId::new(hex::decode(
                    &receipt.operation.0,
                )?))
                .await?;
            let initial_repo = loader.load_at(&initial).await?;
            let initial_wc = initial_repo
                .view()
                .get_wc_commit_id(WorkspaceName::DEFAULT)
                .context("initial attempt WC missing")?;
            let commit = loader.store().get_commit(initial_wc)?;
            ensure!(
                CommitIdentity::from_jj(&commit) == receipt.working_copy
                    && commit.parent_ids() == [baseline.id().clone()]
                    && receipt.working_copy.tree == config.view.attempt.baseline.tree,
                "private readiness does not prove exact initial history"
            );
            let mut pending = vec![heads[0].clone()];
            let mut visited = std::collections::BTreeSet::new();
            let mut found = false;
            while let Some(id) = pending.pop() {
                if !visited.insert(id.clone()) {
                    continue;
                }
                ensure!(
                    visited.len() <= 4096,
                    "private operation ancestry inspection exceeds4096 operations"
                );
                if id.hex() == receipt.operation.0 {
                    found = true;
                    break;
                }
                pending.extend(
                    loader
                        .load_operation(&id)
                        .await?
                        .parent_ids()
                        .iter()
                        .cloned(),
                );
            }
            ensure!(
                found,
                "current private operation is unrelated to attempt readiness"
            );
            RudderWorkingCopyFactory::new(view.clone(), None).load_working_copy(
                loader.store().clone(),
                config.view.private_repository.clone(),
                config.view.working_copy_state(),
                &settings,
            )?;
            view.validate_authority(&config.view.attempt)?;
            return Ok(receipt);
        }
        let state = config.view.working_copy_state().join("rudder-state.json");
        if state.try_exists()?
            && fs::read_to_string(config.view.working_copy_state().join("type"))? == "local"
        {
            reconcile_prepared_working_copy(
                &config.view.private_repository,
                &config.view.attempt,
                &settings,
                view.clone(),
            )
            .await?;
        } else if !state.try_exists()? {
            let baseline_tree = config
                .view
                .attempt
                .baseline
                .tree
                .to_jj(repo.store().clone())?;
            let observed = view.prepare_baseline(&baseline_tree, None)?;
            bind_prepared_working_copy(
                &config.view.private_repository,
                &config.view.attempt,
                &settings,
                view.clone(),
                observed,
            )
            .await?;
        }
        let receipt = initialize_attempt_history(
            &config.view.private_repository,
            &config.view.attempt,
            &settings,
            view.clone(),
        )
        .await?;
        view.validate_authority(&config.view.attempt)?;
        Ok(receipt)
    })
}
