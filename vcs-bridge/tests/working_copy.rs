#![cfg(feature = "jj")]
mod common;
use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use common::*;
use jj_lib::backend::Backend;
use jj_lib::commit::Commit;
use jj_lib::git_backend::GitBackend;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::store::Store;
use jj_lib::working_copy::{CheckoutStats, SnapshotOptions, SnapshotStats};
use jj_lib::{
    settings::UserSettings,
    signing::Signer,
    workspace::{Workspace, WorkspaceInitError},
};
use rudder_vcs_bridge::{working_copy::*, *};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct TestView {
    next_tree: Mutex<Option<MergedTree>>,
    revoked: AtomicBool,
    checkout_enabled: AtomicBool,
    fail_after_start: AtomicBool,
    observe_count: AtomicUsize,
    checkout_count: AtomicUsize,
    previous_cursor_absent: AtomicBool,
    reject_observation: AtomicBool,
}
#[async_trait]
impl WorkspaceView for TestView {
    fn validate_authority(&self, _binding: &AttemptBinding) -> Result<()> {
        ensure!(
            !self.revoked.load(Ordering::SeqCst),
            "attempt authority revoked"
        );
        Ok(())
    }
    fn capabilities(&self, _: &AttemptBinding) -> Result<ViewCapabilities> {
        Ok(ViewCapabilities {
            checkout: self.checkout_enabled.load(Ordering::SeqCst),
            sparse_checkout: false,
        })
    }
    fn bootstrap(&self, binding: &AttemptBinding, store: Arc<Store>) -> Result<ObservedTree> {
        Ok(ObservedTree {
            tree: MergedTree::resolved(store.clone(), store.empty_tree_id().clone()),
            token: observation(binding),
            stats: SnapshotStats::default(),
        })
    }
    fn validate_observation(
        &self,
        binding: &AttemptBinding,
        observation: &ViewToken,
        tree: &JjTreeIdentity,
    ) -> Result<()> {
        observation.validate(binding)?;
        tree.validate()?;
        ensure!(
            !self.reject_observation.load(Ordering::SeqCst),
            "owned observation proof rejected"
        );
        Ok(())
    }
    async fn observe(
        &self,
        binding: &AttemptBinding,
        previous: Option<&ViewToken>,
        _: &JjTreeIdentity,
        _: &SnapshotOptions<'_>,
        store: Arc<Store>,
    ) -> Result<ObservedTree> {
        self.observe_count.fetch_add(1, Ordering::SeqCst);
        self.previous_cursor_absent
            .store(previous.is_none(), Ordering::SeqCst);
        let tree =
            self.next_tree.lock().unwrap().clone().unwrap_or_else(|| {
                MergedTree::resolved(store.clone(), store.empty_tree_id().clone())
            });
        Ok(ObservedTree {
            tree,
            token: observation(binding),
            stats: SnapshotStats::default(),
        })
    }
    async fn checkout(
        &self,
        binding: &AttemptBinding,
        _: &ViewToken,
        _: &str,
        commit: &Commit,
        _: &[RepoPathBuf],
    ) -> Result<AppliedCheckout> {
        self.checkout_count.fetch_add(1, Ordering::SeqCst);
        if self.fail_after_start.load(Ordering::SeqCst) {
            bail!("transport lost after possible root replacement");
        }
        Ok(AppliedCheckout {
            tree: JjTreeIdentity::from_jj(&commit.tree()),
            token: observation(binding),
            stats: CheckoutStats::default(),
        })
    }
    async fn set_sparse_patterns(
        &self,
        _: &AttemptBinding,
        _: &ViewToken,
        _: &str,
        _: &MergedTree,
        _: &[RepoPathBuf],
    ) -> Result<AppliedCheckout> {
        bail!("unsupported sparse profile")
    }
}
fn options() -> SnapshotOptions<'static> {
    SnapshotOptions {
        base_ignores: jj_lib::gitignore::GitIgnoreFile::empty(),
        progress: None,
        start_tracking_matcher: &jj_lib::matchers::EverythingMatcher,
        force_tracking_matcher: &jj_lib::matchers::NothingMatcher,
        max_new_file_size: u64::MAX,
    }
}
async fn setup(root: &Path, view: Arc<TestView>) -> (Workspace, Arc<jj_lib::repo::ReadonlyRepo>) {
    let settings = settings();
    let backend = |s: &UserSettings, path: &Path| {
        Ok(
            Box::new(GitBackend::init_colocated(s, path, Path::new("../../.."))?)
                as Box<dyn Backend>,
        )
    };
    Workspace::init_with_factories(
        &settings,
        root,
        &backend,
        Signer::from_settings(&settings)
            .map_err(WorkspaceInitError::SignInit)
            .unwrap(),
        jj_lib::repo::ReadonlyRepo::default_op_store_initializer(),
        jj_lib::repo::ReadonlyRepo::default_op_heads_store_initializer(),
        jj_lib::repo::ReadonlyRepo::default_index_store_initializer(),
        jj_lib::repo::ReadonlyRepo::default_submodule_store_initializer(),
        &RudderWorkingCopyFactory::new(view, Some(binding())),
        WorkspaceName::DEFAULT.to_owned(),
    )
    .await
    .unwrap()
}
async fn changed_commit(repo: &Arc<jj_lib::repo::ReadonlyRepo>) -> Commit {
    let empty = MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
    let mut tree = jj_lib::merged_tree_builder::MergedTreeBuilder::new(empty);
    let path = RepoPathBuf::from_internal_string("changed").unwrap();
    let file = repo
        .store()
        .write_file(&path, &mut "real object bytes".as_bytes())
        .await
        .unwrap();
    tree.set_or_remove(
        path,
        jj_lib::merge::Merge::resolved(Some(jj_lib::backend::TreeValue::File {
            id: file,
            executable: false,
            copy_id: jj_lib::backend::CopyId::placeholder(),
        })),
    );
    let root = tree.write_tree().await.unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut()
        .new_commit(vec![repo.store().root_commit_id().clone()], root)
        .write()
        .await
        .unwrap()
}

#[test]
fn registered_production_wc_observes_only_injected_view_and_reset_invalidates_cursor() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let view = Arc::new(TestView::default());
        let (workspace, repo) = setup(dir.path(), view.clone()).await;
        assert_eq!(workspace.working_copy().name(), TYPE_NAME);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".jj/working_copy/type")).unwrap(),
            TYPE_NAME
        );
        // A real source file on disk is intentionally invisible to this fake
        // provider, proving the production adapter does not scan LocalWC.
        std::fs::write(dir.path().join("must-not-scan"), "disk bytes").unwrap();
        let mut locked = workspace.working_copy().start_mutation().unwrap();
        let (tree, _) = locked.snapshot(&options()).await.unwrap();
        assert_eq!(
            tree.tree_ids().as_resolved(),
            Some(repo.store().empty_tree_id())
        );
        let wc = repo
            .store()
            .get_commit(
                repo.view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        locked.reset(&wc).await.unwrap();
        locked.snapshot(&options()).await.unwrap();
        assert!(view.previous_cursor_absent.load(Ordering::SeqCst));
        let updated = locked.finish(repo.op_id().clone()).await.unwrap();
        view.revoked.store(true, Ordering::SeqCst);
        assert!(updated.start_mutation().is_err());
    });
}

#[test]
fn unsupported_checkout_is_definite_but_started_transport_failure_remains_uncertain() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let view = Arc::new(TestView::default());
        let (workspace, repo) = setup(dir.path(), view.clone()).await;
        let changed = changed_commit(&repo).await;
        let mut locked = workspace.working_copy().start_mutation().unwrap();
        assert!(locked.check_out(&changed).await.is_err());
        assert!(
            pending_mutation(&dir.path().join(".jj/working_copy"))
                .unwrap()
                .is_none()
        );
        assert_eq!(view.checkout_count.load(Ordering::SeqCst), 0);
        drop(locked);
        view.checkout_enabled.store(true, Ordering::SeqCst);
        view.fail_after_start.store(true, Ordering::SeqCst);
        let mut locked = workspace.working_copy().start_mutation().unwrap();
        assert!(locked.check_out(&changed).await.is_err());
        let pending = pending_mutation(&dir.path().join(".jj/working_copy"))
            .unwrap()
            .unwrap();
        assert_eq!(
            pending.target_tree,
            JjTreeIdentity::from_jj(&changed.tree())
        );
        assert!(
            locked.finish(repo.op_id().clone()).await.is_err(),
            "finish may not erase an uncertain external start"
        );
        assert!(workspace.working_copy().start_mutation().is_err());
        assert_eq!(
            view.checkout_count.load(Ordering::SeqCst),
            1,
            "never replay uncertain checkout"
        );
    });
}

#[test]
fn successful_checkout_clears_pending_only_with_durable_finish() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let view = Arc::new(TestView::default());
        view.checkout_enabled.store(true, Ordering::SeqCst);
        let (workspace, repo) = setup(dir.path(), view).await;
        let changed = changed_commit(&repo).await;
        let mut locked = workspace.working_copy().start_mutation().unwrap();
        locked.check_out(&changed).await.unwrap();
        assert!(
            pending_mutation(&dir.path().join(".jj/working_copy"))
                .unwrap()
                .is_some()
        );
        let updated = locked.finish(repo.op_id().clone()).await.unwrap();
        assert_eq!(
            JjTreeIdentity::from_jj(updated.tree().unwrap()),
            JjTreeIdentity::from_jj(&changed.tree())
        );
        assert!(
            pending_mutation(&dir.path().join(".jj/working_copy"))
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn metadata_reset_cannot_turn_an_unobserved_same_tree_checkout_into_success() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let view = Arc::new(TestView::default());
        let (workspace, repo) = setup(dir.path(), view.clone()).await;
        let changed = changed_commit(&repo).await;
        let mut locked = workspace.working_copy().start_mutation().unwrap();
        locked.reset(&changed).await.unwrap();
        let error = locked.check_out(&changed).await.unwrap_err();
        assert!(error.to_string().contains("snapshot"));
        assert_eq!(view.checkout_count.load(Ordering::SeqCst), 0);
        assert!(
            pending_mutation(&dir.path().join(".jj/working_copy"))
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn a_checkout_result_without_owned_source_proof_keeps_its_uncertain_nonce() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let view = Arc::new(TestView::default());
        view.checkout_enabled.store(true, Ordering::SeqCst);
        let (workspace, repo) = setup(dir.path(), view.clone()).await;
        let changed = changed_commit(&repo).await;
        let mut locked = workspace.working_copy().start_mutation().unwrap();
        view.reject_observation.store(true, Ordering::SeqCst);
        assert!(locked.check_out(&changed).await.is_err());
        assert!(
            pending_mutation(&dir.path().join(".jj/working_copy"))
                .unwrap()
                .is_some()
        );
        assert!(locked.finish(repo.op_id().clone()).await.is_err());
        assert_eq!(view.checkout_count.load(Ordering::SeqCst), 1);
    });
}

#[cfg(feature = "local-development")]
async fn two_metadata_only_attempts(root: &Path, kernel_readonly: bool) {
    use rudder_vcs_bridge::{attempt::*, seed::*};
    let view = Arc::new(TestView::default());
    let original = root.join("prepared");
    std::fs::create_dir(&original).unwrap();
    let (workspace, repo) = setup(&original, view.clone()).await;
    let baseline = repo
        .store()
        .get_commit(
            repo.view()
                .get_wc_commit_id(WorkspaceName::DEFAULT)
                .unwrap(),
        )
        .unwrap();
    let mut binding = binding();
    binding.baseline = CommitIdentity::from_jj(&baseline);
    binding.baseline_operation = JjOperationId(repo.op_id().hex());
    let manifest = prepare_metadata_template(
        &original,
        &root.join("seed"),
        binding.baseline.clone(),
        binding.baseline_operation.clone(),
    )
    .unwrap();
    let objects = root.join("seed/objects");
    if kernel_readonly {
        for args in [
            vec!["--make-rprivate".to_string(), "/".into()],
            vec![
                "--bind".into(),
                objects.display().to_string(),
                objects.display().to_string(),
            ],
            vec![
                "-o".into(),
                "remount,bind,ro".into(),
                objects.display().to_string(),
                objects.display().to_string(),
            ],
        ] {
            assert!(
                std::process::Command::new("mount")
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert_eq!(
            std::fs::write(objects.join("forbidden-write"), b"no")
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EROFS)
        );
    }
    let cas = rudder_engine::store::LocalStore::open(root.join("seed-cas")).unwrap();
    let staging = cas
        .begin_staging(std::time::Duration::from_secs(60))
        .unwrap();
    let seed_root = cas
        .import_tree(
            root.join("seed"),
            rudder_engine::protocol::CheckpointConsistency::Quiesced,
        )
        .unwrap();
    cas.pin("immutable-vcs-seed", &seed_root).unwrap();
    cas.abandon_staging(&staging.id).unwrap();
    binding.seed = seed_root.clone();
    let verified = if kernel_readonly {
        VerifiedMetadataTemplate::open_retained(&cas, &seed_root, &root.join("seed"), &objects)
            .unwrap()
    } else {
        VerifiedMetadataTemplate::open_retained_development_integrity_only(
            &cas,
            &seed_root,
            &root.join("seed"),
            &objects,
        )
        .unwrap()
    };
    assert_eq!(verified.manifest(), &manifest);
    let mut a = binding.clone();
    a.run_id = "a-run".into();
    a.attempt_id = "a".into();
    a.view_id = "a-view".into();
    let mut b = binding.clone();
    b.run_id = "b-run".into();
    b.attempt_id = "b".into();
    b.view_id = "b-view".into();
    let first = fork_private_attempt(
        &verified,
        &root.join("a"),
        a.clone(),
        &observation(&a),
        &settings(),
        view.clone(),
    )
    .await
    .unwrap();
    let second = fork_private_attempt(
        &verified,
        &root.join("b"),
        b.clone(),
        &observation(&b),
        &settings(),
        view.clone(),
    )
    .await
    .unwrap();
    assert_ne!(first.working_copy.change, second.working_copy.change);
    assert_ne!(first.working_copy.change, binding.baseline.change);
    assert_eq!(
        first.working_copy.parents,
        vec![binding.baseline.commit.clone()]
    );
    assert_eq!(second.working_copy.parents, first.working_copy.parents);
    assert_eq!(first.working_copy.tree, binding.baseline.tree);
    assert_eq!(
        initialize_attempt_history(&root.join("a"), &a, &settings(), view.clone())
            .await
            .unwrap(),
        first,
        "retry reconciles same private op without minting another change"
    );
    assert_eq!(
        view.observe_count.load(Ordering::SeqCst),
        0,
        "fork never snapshots or enumerates source"
    );
    assert_eq!(
        view.checkout_count.load(Ordering::SeqCst),
        0,
        "metadata-only fork never materializes a tree"
    );
    let mut wrong = a.clone();
    wrong.seed = reference(
        rudder_engine::protocol::ObjectKind::Directory,
        "unrelated-seed",
    );
    assert!(
        fork_private_attempt(
            &verified,
            &root.join("wrong-seed"),
            wrong.clone(),
            &observation(&wrong),
            &settings(),
            view.clone()
        )
        .await
        .is_err()
    );
    assert!(!root.join("wrong-seed").exists());
    assert_eq!(
        workspace.working_copy().operation_id().hex(),
        binding.baseline_operation.0
    );
    if kernel_readonly {
        assert!(
            std::process::Command::new("umount")
                .arg(&objects)
                .status()
                .unwrap()
                .success()
        );
    }
}

#[cfg(feature = "local-development")]
#[test]
fn private_attempts_remint_only_their_new_working_copy_changes_without_scanning_source() {
    let root = tempfile::tempdir().unwrap();
    pollster::block_on(two_metadata_only_attempts(root.path(), false));
}

#[cfg(all(feature = "local-development", target_os = "linux"))]
#[test]
#[ignore = "run in a new explicit user/mount namespace with RUDDER_VCS_PARENT_MNT_NS"]
fn private_readonly_mount_qualifies_real_metadata_only_attempt_forks() {
    let parent = std::env::var("RUDDER_VCS_PARENT_MNT_NS")
        .expect("explicit caller namespace witness required");
    let current = std::fs::read_link("/proc/self/ns/mnt").unwrap();
    assert_ne!(
        current.to_string_lossy(),
        parent,
        "never mount in the caller namespace"
    );
    let root = tempfile::tempdir().unwrap();
    pollster::block_on(two_metadata_only_attempts(root.path(), true));
}
