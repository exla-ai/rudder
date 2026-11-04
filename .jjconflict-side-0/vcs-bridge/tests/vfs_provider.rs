#![cfg(feature = "jj")]
mod common;

use anyhow::{Result, ensure};
use common::{binding, settings};
use jj_lib::git_backend::GitBackend;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::working_copy::{SnapshotOptions, WorkingCopyFactory};
use jj_lib::workspace::Workspace;
use rudder_engine::directory::{LocalDirectoryStore, PagedDirectory};
use rudder_engine::protocol::canonical_json;
use rudder_engine::store::{ImportPolicy, LocalStore};
use rudder_engine::vfs::{NodeKind, ObservationBudget, VfsManager};
use rudder_vcs_bridge::projection::{project_source, write_versioned_directory};
use rudder_vcs_bridge::projection_io::ProjectionBudget;
use rudder_vcs_bridge::vfs_provider::*;
use rudder_vcs_bridge::working_copy::*;
use rudder_vcs_bridge::*;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

struct Authority {
    binding: AttemptBinding,
    revoked: AtomicBool,
}
impl AttemptAuthority for Authority {
    fn validate(&self, binding: &AttemptBinding) -> Result<()> {
        ensure!(
            !self.revoked.load(Ordering::SeqCst) && *binding == self.binding,
            "trusted attempt authority was revoked"
        );
        Ok(())
    }
}
fn options() -> SnapshotOptions<'static> {
    SnapshotOptions {
        base_ignores: GitIgnoreFile::empty(),
        progress: None,
        start_tracking_matcher: &EverythingMatcher,
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size: u64::MAX,
    }
}
fn persisted_observation(metadata: &Path) -> LiveObservation {
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(metadata.join(".jj/working_copy/rudder-state.json")).unwrap(),
    )
    .unwrap();
    serde_json::from_value(state["observation"].clone()).unwrap()
}

#[test]
fn owning_view_binds_a_real_nonempty_wc_and_snapshots_edits_with_post_commit_pin_adoption() {
    for lose_adoption_receipt in [false, true] {
        pollster::block_on(async {
            let fixture = tempfile::tempdir().unwrap();
            let metadata = fixture.path().join("private-attempt");
            std::fs::create_dir(&metadata).unwrap();
            let (_local_wc, repo) = Workspace::init_internal_git(&settings(), &metadata)
                .await
                .unwrap();
            let store = LocalStore::open(fixture.path().join("cas")).unwrap();
            let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
            let empty_root = PagedDirectory::new(&LocalDirectoryStore::new(&store, &stage.id))
                .build(0o755, [])
                .unwrap();
            store
                .publish(&stage.id, "prepared-fixture", &empty_root)
                .unwrap();
            let manager = VfsManager::open(fixture.path().join("vfs"), store.clone()).unwrap();
            manager.prepare_paged(&empty_root).unwrap();
            let mut binding = binding();
            binding.view_id = uuid::Uuid::new_v4().to_string();
            let view = manager
                .fork_paged(&binding.view_id, &empty_root, binding.owner.clone())
                .unwrap();
            let file = view
                .create(1, b"source.ts", NodeKind::File, 0o644, b"")
                .unwrap();
            view.write(file.ino, 0, b"export const n = 1;\n", false)
                .unwrap();
            let prepared = view
                .create(1, b"node_modules", NodeKind::Directory, 0o755, b"")
                .unwrap();
            let scratch = view
                .create(prepared.ino, b"generated", NodeKind::File, 0o600, b"")
                .unwrap();
            view.write(scratch.ino, 0, b"excluded prepared inputs", false)
                .unwrap();
            let exclusions = ImportPolicy {
                excluded_paths: vec![vec![b"node_modules".to_vec()]],
            };
            let policy = SourceProjectionPolicy::from_import_policy(&exclusions).unwrap();
            binding.projection_policy = store.put_blob(&canonical_json(&policy).unwrap()).unwrap();
            let initial_nonce = uuid::Uuid::new_v4().to_string();
            let initial = view
                .observe_source(&exclusions, &initial_nonce, &ObservationBudget::default())
                .unwrap();
            let empty_tree =
                MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
            let projected = project_source(
                store.clone(),
                &initial.directory,
                &empty_tree,
                &binding.projection_policy,
                None,
                None,
                ProjectionBudget::default(),
            )
            .await
            .unwrap();
            let baseline = repo
                .store()
                .get_commit(
                    repo.view()
                        .get_wc_commit_id(WorkspaceName::DEFAULT)
                        .unwrap(),
                )
                .unwrap();
            let mut tx = repo.start_transaction();
            let prepared_commit = tx
                .repo_mut()
                .rewrite_commit(&baseline)
                .set_tree(projected.tree.clone())
                .write()
                .await
                .unwrap();
            tx.repo_mut()
                .set_wc_commit(
                    WorkspaceName::DEFAULT.to_owned(),
                    prepared_commit.id().clone(),
                )
                .unwrap();
            tx.repo_mut().rebase_descendants().await.unwrap();
            let repo = tx
                .commit("prepare nonempty private metadata fixture")
                .await
                .unwrap();
            binding.baseline = CommitIdentity::from_jj(&prepared_commit);
            binding.baseline_operation = JjOperationId(repo.op_id().hex());
            binding.seed = empty_root;
            view.release_observation(&initial_nonce).unwrap();
            let authority = Arc::new(Authority {
                binding: binding.clone(),
                revoked: AtomicBool::new(false),
            });
            let provider_path = fixture.path().join("engine-private-provider");
            let provider = Arc::new(
                VfsWorkspaceView::new(
                    view.clone(),
                    binding.clone(),
                    authority.clone(),
                    &provider_path,
                    repo.store()
                        .backend_impl::<GitBackend>()
                        .unwrap()
                        .git_repo_path(),
                    policy,
                    ObservationBudget::default(),
                    ProjectionBudget::default(),
                )
                .unwrap(),
            );
            let observed = provider
                .prepare_baseline(&prepared_commit.tree(), None)
                .unwrap();
            let old = observed.token.clone();
            let receipt = if lose_adoption_receipt {
                let cursor = view.dirty_cursor().unwrap();
                std::fs::create_dir(provider_path.join("adopted.json")).unwrap();
                assert!(
                    bind_prepared_working_copy(
                        &metadata,
                        &binding,
                        &settings(),
                        provider.clone(),
                        observed
                    )
                    .await
                    .is_err()
                );
                assert_eq!(
                    std::fs::read_to_string(metadata.join(".jj/working_copy/type")).unwrap(),
                    "local"
                );
                assert_eq!(persisted_observation(&metadata), old);
                std::fs::remove_dir(provider_path.join("adopted.json")).unwrap();
                let receipt = reconcile_prepared_working_copy(
                    &metadata,
                    &binding,
                    &settings(),
                    provider.clone(),
                )
                .await
                .unwrap();
                assert_eq!(view.dirty_cursor().unwrap(), cursor);
                assert_eq!(receipt.observation, old);
                receipt
            } else {
                bind_prepared_working_copy(
                    &metadata,
                    &binding,
                    &settings(),
                    provider.clone(),
                    observed,
                )
                .await
                .unwrap()
            };
            assert_eq!(receipt.commit.change, binding.baseline.change);
            assert_eq!(receipt.observation, old);
            assert_eq!(
                std::fs::read_to_string(metadata.join(".jj/working_copy/type")).unwrap(),
                TYPE_NAME
            );
            let factory = RudderWorkingCopyFactory::new(provider.clone(), None);
            let wc = factory
                .load_working_copy(
                    repo.store().clone(),
                    metadata.clone(),
                    metadata.join(".jj/working_copy"),
                    &settings(),
                )
                .unwrap();
            assert_eq!(
                JjTreeIdentity::from_jj(wc.tree().unwrap()),
                binding.baseline.tree
            );
            let mut locked = wc.start_mutation().unwrap();
            view.write(file.ino, 0, b"export const n = 2;\n", false)
                .unwrap();
            let cursor_before_snapshot = view.dirty_cursor().unwrap();
            let (tree, _) = locked.snapshot(&options()).await.unwrap();
            assert_eq!(view.dirty_cursor().unwrap(), cursor_before_snapshot);
            assert_ne!(JjTreeIdentity::from_jj(&tree), binding.baseline.tree);
            // Source was read through the owning VFS, never materialized here.
            assert!(!metadata.join("source.ts").exists());
            assert!(store.retention().unwrap().pins.contains_key(&old.retention));
            let mut tx = repo.start_transaction();
            let changed_commit = tx
                .repo_mut()
                .rewrite_commit(&prepared_commit)
                .set_tree(tree.clone())
                .write()
                .await
                .unwrap();
            assert_eq!(changed_commit.change_id(), prepared_commit.change_id());
            tx.repo_mut()
                .set_wc_commit(
                    WorkspaceName::DEFAULT.to_owned(),
                    changed_commit.id().clone(),
                )
                .unwrap();
            tx.repo_mut().rebase_descendants().await.unwrap();
            let changed_repo = tx.commit("snapshot the owned VFS source").await.unwrap();
            let adopted_file = provider_path.join("adopted.json");
            let old_adoption = std::fs::read(&adopted_file).unwrap();
            if lose_adoption_receipt {
                // Scoped I/O fault after the WC fsync: the adoption path cannot
                // be read, while the new snapshot/operation can still persist.
                std::fs::remove_file(&adopted_file).unwrap();
                std::fs::create_dir(&adopted_file).unwrap();
            }
            let finished = locked.finish(changed_repo.op_id().clone()).await;
            let current = persisted_observation(&metadata);
            assert_ne!(current.request_nonce, old.request_nonce);
            assert!(
                store
                    .retention()
                    .unwrap()
                    .pins
                    .contains_key(&current.retention)
            );
            if lose_adoption_receipt {
                assert!(finished.is_err());
                assert!(store.retention().unwrap().pins.contains_key(&old.retention));
                std::fs::remove_dir(&adopted_file).unwrap();
                std::fs::write(&adopted_file, old_adoption).unwrap();
                // Loading persisted exact state retries retention bookkeeping,
                // not source observation or history mutation.
                let loaded = factory
                    .load_working_copy(
                        changed_repo.store().clone(),
                        metadata.clone(),
                        metadata.join(".jj/working_copy"),
                        &settings(),
                    )
                    .unwrap();
                assert_eq!(
                    JjTreeIdentity::from_jj(loaded.tree().unwrap()),
                    JjTreeIdentity::from_jj(&tree)
                );
                assert_eq!(persisted_observation(&metadata), current);
            } else {
                assert!(finished.is_ok());
            }
            assert!(!store.retention().unwrap().pins.contains_key(&old.retention));
            assert!(
                store
                    .retention()
                    .unwrap()
                    .pins
                    .contains_key(&current.retention)
            );
            let work = provider.work_for(&current).unwrap();
            assert!(work.source_entries_visited > 0 && work.git_blob_bytes_submitted > 0);
            let projection = provider.projection_for(&current).unwrap();
            assert!(!projection.paths.contains_key("node_modules/generated"));
            let selected_stage = store.begin_staging(Duration::from_secs(600)).unwrap();
            let versioned =
                write_versioned_directory(&store, &selected_stage.id, &projection).unwrap();
            store
                .publish(&selected_stage.id, "versioned-fixture", &versioned)
                .unwrap();
            assert_eq!(versioned, current.directory);
            let wc = factory
                .load_working_copy(
                    changed_repo.store().clone(),
                    metadata.clone(),
                    metadata.join(".jj/working_copy"),
                    &settings(),
                )
                .unwrap();
            let mut locked = wc.start_mutation().unwrap();
            let (same_tree, _) = locked.snapshot(&options()).await.unwrap();
            assert_eq!(
                JjTreeIdentity::from_jj(&same_tree),
                JjTreeIdentity::from_jj(&tree)
            );
            let wc = locked.finish(changed_repo.op_id().clone()).await.unwrap();
            let latest = persisted_observation(&metadata);
            assert_eq!(
                provider.work_for(&latest).unwrap().git_blob_bytes_submitted,
                0
            );
            let mut incompatible = MergedTreeBuilder::new(tree.clone());
            incompatible.set_or_remove(
                RepoPathBuf::from_internal_string("node_modules/already-tracked.ts").unwrap(),
                tree.path_value(&RepoPathBuf::from_internal_string("source.ts").unwrap())
                    .await
                    .unwrap(),
            );
            let incompatible = incompatible.write_tree().await.unwrap();
            let error = provider
                .observe(
                    &binding,
                    Some(&latest),
                    &JjTreeIdentity::from_jj(&incompatible),
                    &options(),
                    changed_repo.store().clone(),
                )
                .await;
            assert!(error.err().unwrap().to_string().contains("already tracked"));
            let before_unsupported = view.dirty_cursor().unwrap();
            let mut locked = wc.start_mutation().unwrap();
            assert!(
                locked
                    .check_out(&baseline)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("unsupported")
            );
            assert_eq!(view.dirty_cursor().unwrap(), before_unsupported);
            assert!(
                pending_mutation(&metadata.join(".jj/working_copy"))
                    .unwrap()
                    .is_none()
            );
            drop(locked);
            let mut forged = latest.clone();
            forged.directory = binding.seed.clone();
            assert!(
                provider
                    .validate_observation(&binding, &forged, &JjTreeIdentity::from_jj(&tree))
                    .is_err()
            );
            authority.revoked.store(true, Ordering::SeqCst);
            assert!(
                factory
                    .load_working_copy(
                        changed_repo.store().clone(),
                        metadata.clone(),
                        metadata.join(".jj/working_copy"),
                        &settings()
                    )
                    .is_err()
            );
            assert!(
                provider
                    .prepare_baseline(&prepared_commit.tree(), None)
                    .is_err()
            );
            assert_eq!(
                view.read(scratch.ino, 0, 100).unwrap(),
                b"excluded prepared inputs"
            );
        });
    }
}

#[test]
fn policy_identity_and_binding_fail_closed_without_an_owned_provider_receipt() {
    let policy = SourceProjectionPolicy::from_import_policy(&ImportPolicy {
        excluded_paths: vec![
            vec![b"target".to_vec()],
            vec![b"node_modules".to_vec()],
            vec![b"target".to_vec()],
        ],
    })
    .unwrap();
    assert_eq!(policy.excluded_paths_hex.len(), 2);
    assert_eq!(
        policy.reference().unwrap(),
        SourceProjectionPolicy::from_import_policy(&policy.import_policy().unwrap())
            .unwrap()
            .reference()
            .unwrap()
    );
    let mut malformed = policy.clone();
    malformed.excluded_paths_hex.reverse();
    assert!(malformed.reference().is_err());
    assert!(
        SourceProjectionPolicy::from_import_policy(&ImportPolicy {
            excluded_paths: vec![vec![b"..".to_vec()]]
        })
        .is_err()
    );
}
