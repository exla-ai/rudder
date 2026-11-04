#![cfg(feature = "jj")]
mod common;

use common::settings;
use jj_lib::backend::TreeValue;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, FilesMatcher};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::working_copy::{SnapshotOptions, UntrackedReason};
use jj_lib::workspace::Workspace;
use rudder_engine::content::{self, StagedContentStore};
use rudder_engine::directory::{DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory};
use rudder_engine::protocol::{CheckpointConsistency, ObjectKind, ObjectRef};
use rudder_engine::snapshot::SourceV2;
use rudder_engine::store::LocalStore;
use rudder_vcs_bridge::JjTreeIdentity;
use rudder_vcs_bridge::projection::*;
use rudder_vcs_bridge::projection_io::ProjectionBudget;
use std::time::Duration;

fn path(s: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(s).unwrap()
}
fn put_file(store: &LocalStore, stage: &str, bytes: &[u8], mode: u32) -> EntryNode {
    let adapter = StagedContentStore::new(store, stage).unwrap();
    let object = content::import_file(&adapter, &mut &bytes[..]).unwrap();
    EntryNode::File {
        object,
        logical_size: bytes.len() as u64,
        mode,
    }
}
fn directory(store: &LocalStore, stage: &str, entries: Vec<(&[u8], EntryNode)>) -> ObjectRef {
    PagedDirectory::new(&LocalDirectoryStore::new(store, stage))
        .build(
            0o755,
            entries
                .into_iter()
                .map(|(n, node)| DirectoryEntry::new(n, node).unwrap()),
        )
        .unwrap()
}
fn source_bytes(store: &LocalStore, record: &ProjectionRecord, name: &[u8]) -> Vec<u8> {
    let entry = rudder_engine::tree::lookup(store, &record.directory, name)
        .unwrap()
        .unwrap();
    let EntryNode::File {
        object,
        logical_size,
        ..
    } = entry.node
    else {
        panic!("file expected")
    };
    content::read_file_range(store, &object, 0, logical_size as usize).unwrap()
}
async fn git_bytes(tree: &MergedTree, s: &str) -> Vec<u8> {
    let value = tree.path_value(&path(s)).await.unwrap();
    let Some(Some(TreeValue::File { id, .. })) = value.as_resolved() else {
        panic!("resolved file expected: {value:?}")
    };
    let backend = tree
        .store()
        .backend_impl::<jj_lib::git_backend::GitBackend>()
        .unwrap();
    backend
        .git_repo()
        .find_object(gix::ObjectId::try_from(id.as_bytes()).unwrap())
        .unwrap()
        .try_into_blob()
        .unwrap()
        .take_data()
}

#[test]
fn nonempty_files_modes_symlinks_and_utf8_paths_roundtrip_without_source_walk() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("private-jj");
        std::fs::create_dir(&repo_dir).unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&settings(), &repo_dir)
            .await
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = store.put_blob(b"exact versioned leaves v1").unwrap();
        let nested = directory(
            &store,
            &stage.id,
            vec![(
                "café source.ts".as_bytes(),
                put_file(&store, &stage.id, b"export const n = 1;\n", 0o644),
            )],
        );
        let source = directory(
            &store,
            &stage.id,
            vec![
                (b"src", EntryNode::Directory { object: nested }),
                (
                    b"run",
                    put_file(&store, &stage.id, b"#!/bin/sh\necho qualified\n", 0o755),
                ),
                (b"empty", put_file(&store, &stage.id, b"", 0o644)),
                (
                    b"link",
                    EntryNode::Symlink {
                        target_hex: hex::encode("src/café source.ts"),
                    },
                ),
            ],
        );
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        let projected = project_source(
            store.clone(),
            &source,
            &empty,
            &policy,
            None,
            None,
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            git_bytes(&projected.tree, "src/café source.ts").await,
            b"export const n = 1;\n"
        );
        assert!(matches!(
            projected
                .tree
                .path_value(&path("run"))
                .await
                .unwrap()
                .as_resolved(),
            Some(Some(TreeValue::File {
                executable: true,
                ..
            }))
        ));
        assert!(matches!(
            projected
                .tree
                .path_value(&path("link"))
                .await
                .unwrap()
                .as_resolved(),
            Some(Some(TreeValue::Symlink(_)))
        ));
        let baseline = repo
            .store()
            .get_commit(
                repo.view()
                    .get_wc_commit_id(jj_lib::ref_name::WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        let mut tx = repo.start_transaction();
        let rewritten = tx
            .repo_mut()
            .rewrite_commit(&baseline)
            .set_tree(projected.tree.clone())
            .write()
            .await
            .unwrap();
        assert_eq!(rewritten.change_id(), baseline.change_id());
        let (materialized, _) = materialize_source(
            store.clone(),
            &stage.id,
            &projected.tree,
            &policy,
            None,
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(materialized.directory, source);
        let again = project_source(
            store.clone(),
            &source,
            &projected.tree,
            &policy,
            None,
            Some(&projected.record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            JjTreeIdentity::from_jj(&again.tree),
            JjTreeIdentity::from_jj(&projected.tree)
        );
        assert_eq!(again.work.cas_content_bytes, 0);
        assert_eq!(again.work.git_blob_bytes_submitted, 0);
        assert_eq!(again.work.reused_leaf_values, 4);
        assert!(again.work.source_entries_visited >= 5 && again.work.jj_entries_visited >= 4);
        // Merkle file reuse does not claim an O(1) namespace snapshot.
        assert!(again.work.cas_metadata_reads > 0);
        assert!(!repo_dir.join("src").exists());
        let source_root = store
            .put_metadata(
                ObjectKind::SourceRoot,
                &SourceV2 {
                    format_version: 2,
                    consistency: CheckpointConsistency::Quiesced,
                    directory: source.clone(),
                    projection_policy: policy.clone(),
                },
            )
            .unwrap();
        let verified = verify_versioned_source(
            store.clone(),
            &stage.id,
            &source_root,
            &policy,
            &projected.tree,
            None,
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(verified.receipt().work.reused_leaf_values, 0);
        assert!(verified.receipt().work.git_blob_bytes_submitted > 0);
        assert_eq!(verified.evidence().kind, ObjectKind::ActionResult);
        assert_eq!(
            store.retention().unwrap().pins[verified.retention_pin()],
            *verified.evidence()
        );
        assert!(verified.release_after_journal_retention().is_err());
        assert!(
            verify_versioned_source(
                store.clone(),
                &stage.id,
                &source_root,
                &policy,
                &empty,
                None,
                ProjectionBudget::default()
            )
            .await
            .is_err()
        );
        // A serializable optimization record cannot substitute a proof. Force
        // every file through CAS→Git mapping even if cache claims are forged.
        let mut forged = projected.record.clone();
        forged.tree = JjTreeIdentity::from_jj(&empty);
        assert!(
            verify_versioned_source(
                store.clone(),
                &stage.id,
                &source_root,
                &policy,
                &empty,
                Some(&forged),
                ProjectionBudget::default()
            )
            .await
            .is_err()
        );
        store
            .pin("simulated-journal-evidence-owner", verified.evidence())
            .unwrap();
        verified.release_after_journal_retention().unwrap();
        assert!(
            !store
                .retention()
                .unwrap()
                .pins
                .contains_key(verified.retention_pin())
        );
    });
}

#[test]
fn large_chunked_and_legacy_files_stream_into_real_git_objects_with_bounded_jj_buffers() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = store.put_blob(b"streamed logical file bytes").unwrap();
        let bytes = vec![b'x'; 5 * 1024 * 1024 + 17];
        let chunked = put_file(&store, &stage.id, &bytes, 0o644);
        assert!(
            matches!(&chunked, EntryNode::File { object, .. } if object.kind == ObjectKind::File)
        );
        let legacy = store.put_blob(&bytes).unwrap();
        let source = directory(
            &store,
            &stage.id,
            vec![
                (b"chunked", chunked),
                (
                    b"legacy",
                    EntryNode::File {
                        object: legacy,
                        logical_size: bytes.len() as u64,
                        mode: 0o644,
                    },
                ),
            ],
        );
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        let budget = ProjectionBudget {
            max_jj_buffer_bytes: 128,
            ..ProjectionBudget::default()
        };
        let mapped = project_source(
            store.clone(),
            &source,
            &empty,
            &policy,
            None,
            None,
            budget.clone(),
        )
        .await
        .unwrap();
        let a = mapped.tree.path_value(&path("chunked")).await.unwrap();
        let b = mapped.tree.path_value(&path("legacy")).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(mapped.work.jj_buffer_bytes_admitted, 0);
        assert_eq!(mapped.work.git_blob_bytes_submitted, 2 * bytes.len() as u64);
        assert_eq!(mapped.work.cas_content_bytes, 2 * bytes.len() as u64);
        // Reverse materialization explicitly rejects full-blob jj buffering
        // beyond its separate profile, rather than ignoring the bound.
        assert!(
            materialize_source(store, &stage.id, &mapped.tree, &policy, None, budget)
                .await
                .is_err()
        );
    });
}

#[test]
fn current_ignore_auto_force_size_and_tracked_rules_are_evaluated_each_snapshot() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = store.put_blob(b"jj snapshot options semantics v1").unwrap();
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        let initial = directory(
            &store,
            &stage.id,
            vec![(b"tracked.log", put_file(&store, &stage.id, b"old", 0o644))],
        );
        let tracked = project_source(
            store.clone(),
            &initial,
            &empty,
            &policy,
            None,
            None,
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        let ignored = directory(
            &store,
            &stage.id,
            vec![
                (
                    b".gitignore",
                    put_file(&store, &stage.id, b"!ignored.txt\n", 0o644),
                ),
                (
                    b"ignored.txt",
                    put_file(&store, &stage.id, b"hidden", 0o644),
                ),
            ],
        );
        let source = directory(
            &store,
            &stage.id,
            vec![
                (
                    b".gitignore",
                    put_file(&store, &stage.id, b"*.log\nignored/\n", 0o644),
                ),
                (
                    b"tracked.log",
                    put_file(&store, &stage.id, b"tracked grows beyond limit", 0o644),
                ),
                (b"other.log", put_file(&store, &stage.id, b"hidden", 0o644)),
                (
                    b"forced.log",
                    put_file(&store, &stage.id, b"force bigger than limit", 0o644),
                ),
                (
                    b"large",
                    put_file(&store, &stage.id, b"untracked too large", 0o644),
                ),
                (b"manual", put_file(&store, &stage.id, b"x", 0o644)),
                (b"ignored", EntryNode::Directory { object: ignored }),
            ],
        );
        let force = FilesMatcher::new([path("forced.log")]);
        let start = jj_lib::matchers::DifferenceMatcher::new(
            EverythingMatcher,
            FilesMatcher::new([path("manual")]),
        );
        let options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &start,
            force_tracking_matcher: &force,
            max_new_file_size: 4,
        };
        let projected = project_source(
            store.clone(),
            &source,
            &tracked.tree,
            &policy,
            Some(&options),
            Some(&tracked.record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            git_bytes(&projected.tree, "tracked.log").await,
            b"tracked grows beyond limit"
        );
        assert_eq!(
            git_bytes(&projected.tree, "forced.log").await,
            b"force bigger than limit"
        );
        assert!(
            projected
                .tree
                .path_value(&path("other.log"))
                .await
                .unwrap()
                .is_absent()
        );
        assert!(
            projected
                .tree
                .path_value(&path("ignored/ignored.txt"))
                .await
                .unwrap()
                .is_absent()
        );
        assert!(matches!(
            projected.stats.untracked_paths[&path("manual")],
            UntrackedReason::FileNotAutoTracked
        ));
        assert!(matches!(
            projected.stats.untracked_paths[&path("large")],
            UntrackedReason::FileTooLarge { .. }
        ));
        let all_options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &EverythingMatcher,
            max_new_file_size: u64::MAX,
        };
        let forced_all = project_source(
            store.clone(),
            &source,
            &projected.tree,
            &policy,
            Some(&all_options),
            Some(&projected.record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            git_bytes(&forced_all.tree, "ignored/ignored.txt").await,
            b"hidden"
        );
        assert_eq!(git_bytes(&forced_all.tree, "manual").await, b"x");
        // Same source root, different opaque matcher inputs: no fake policy
        // equality cache skips reevaluation.
        assert!(forced_all.work.cas_metadata_reads > 0);
    });
}

#[test]
fn file_directory_replacements_and_deletions_have_exact_leaf_semantics() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = store.put_blob(b"exact leaves").unwrap();
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        let old_nested = directory(
            &store,
            &stage.id,
            vec![(b"child", put_file(&store, &stage.id, b"old child", 0o644))],
        );
        let initial = directory(
            &store,
            &stage.id,
            vec![
                (b"a", put_file(&store, &stage.id, b"was file", 0o644)),
                (b"b", EntryNode::Directory { object: old_nested }),
                (b"removed", put_file(&store, &stage.id, b"gone", 0o644)),
            ],
        );
        let first = project_source(
            store.clone(),
            &initial,
            &empty,
            &policy,
            None,
            None,
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        let new_nested = directory(
            &store,
            &stage.id,
            vec![(b"child", put_file(&store, &stage.id, b"new child", 0o644))],
        );
        let next = directory(
            &store,
            &stage.id,
            vec![
                (b"a", EntryNode::Directory { object: new_nested }),
                (b"b", put_file(&store, &stage.id, b"now file", 0o755)),
            ],
        );
        let second = project_source(
            store,
            &next,
            &first.tree,
            &policy,
            None,
            Some(&first.record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(git_bytes(&second.tree, "a/child").await, b"new child");
        assert_eq!(git_bytes(&second.tree, "b").await, b"now file");
        assert!(
            second
                .tree
                .path_value(&path("b/child"))
                .await
                .unwrap()
                .is_absent()
        );
        assert!(
            second
                .tree
                .path_value(&path("removed"))
                .await
                .unwrap()
                .is_absent()
        );
        assert_eq!(second.tree.entries().count(), 2);
    });
}

#[test]
fn conflicts_keep_raw_terms_labels_and_long_marker_receipts_across_partial_resolution() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = store.put_blob(b"jj diff conflicts v1").unwrap();
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        let mut terms = Vec::new();
        for bytes in [
            b"common\n<<<<<<< literal\nleft\n".as_slice(),
            b"common\n<<<<<<< literal\nbase\n",
            b"common\n<<<<<<< literal\nright\n",
        ] {
            let root = directory(
                &store,
                &stage.id,
                vec![(b"conflicted", put_file(&store, &stage.id, bytes, 0o644))],
            );
            let tree = project_source(
                store.clone(),
                &root,
                &empty,
                &policy,
                None,
                None,
                ProjectionBudget::default(),
            )
            .await
            .unwrap()
            .tree;
            terms.push(tree.tree_ids().as_resolved().unwrap().clone());
        }
        let labels = vec![
            "left branch".into(),
            "common ancestor".into(),
            "right branch".into(),
        ];
        let conflict = MergedTree::new(
            repo.store().clone(),
            Merge::from_vec(terms),
            ConflictLabels::from_vec(labels.clone()),
        );
        let (record, _) = materialize_source(
            store.clone(),
            &stage.id,
            &conflict,
            &policy,
            None,
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        let marker = record.paths["conflicted"].conflict_marker_len.unwrap();
        assert!(marker > 7);
        let rendered = source_bytes(&store, &record, b"conflicted");
        assert!(String::from_utf8_lossy(&rendered).contains("left branch"));
        let unchanged = project_source(
            store.clone(),
            &record.directory,
            &conflict,
            &policy,
            None,
            Some(&record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            JjTreeIdentity::from_jj(&unchanged.tree),
            JjTreeIdentity::from_jj(&conflict)
        );
        let edited = String::from_utf8(rendered)
            .unwrap()
            .replace("<<<<<<< literal\n", "");
        let changed_source = directory(
            &store,
            &stage.id,
            vec![(
                b"conflicted",
                put_file(
                    &store,
                    &stage.id,
                    format!("edited common prefix\n{edited}").as_bytes(),
                    0o644,
                ),
            )],
        );
        let partial = project_source(
            store.clone(),
            &changed_source,
            &conflict,
            &policy,
            None,
            Some(&record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert!(partial.tree.has_conflict());
        assert_eq!(partial.tree.labels().as_slice(), labels);
        assert_eq!(
            partial.record.paths["conflicted"].conflict_marker_len,
            Some(marker)
        );
        assert_ne!(
            JjTreeIdentity::from_jj(&partial.tree),
            JjTreeIdentity::from_jj(&conflict)
        );
        let edited_again = directory(
            &store,
            &stage.id,
            vec![(
                b"conflicted",
                put_file(
                    &store,
                    &stage.id,
                    format!("another common line\nedited common prefix\n{edited}").as_bytes(),
                    0o644,
                ),
            )],
        );
        let partial_again = project_source(
            store.clone(),
            &edited_again,
            &partial.tree,
            &policy,
            None,
            Some(&partial.record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert!(partial_again.tree.has_conflict());
        assert_eq!(
            partial_again.record.paths["conflicted"].conflict_marker_len,
            Some(marker)
        );
        let source_root = store
            .put_metadata(
                ObjectKind::SourceRoot,
                &SourceV2 {
                    format_version: 2,
                    consistency: CheckpointConsistency::Quiesced,
                    directory: edited_again.clone(),
                    projection_policy: policy.clone(),
                },
            )
            .unwrap();
        let verified = verify_versioned_source(
            store.clone(),
            &stage.id,
            &source_root,
            &policy,
            &partial_again.tree,
            Some(&partial_again.record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert!(verified.receipt().marker_receipt.is_some());
        let mut proof = common::proof(
            common::binding(),
            JjTreeIdentity::from_jj(&partial_again.tree),
        );
        proof.source = source_root;
        proof.binding.projection_policy = policy.clone();
        verified.check_proof(&proof).unwrap();
        let marker_blob = store
            .read_object(verified.receipt().marker_receipt.as_ref().unwrap())
            .unwrap();
        let marker_record: serde_json::Value = serde_json::from_slice(&marker_blob).unwrap();
        assert_eq!(marker_record["lengths"]["conflicted"], marker);
        let resolved_source = directory(
            &store,
            &stage.id,
            vec![(
                b"conflicted",
                put_file(&store, &stage.id, b"human resolved\n", 0o644),
            )],
        );
        let resolved = project_source(
            store.clone(),
            &resolved_source,
            &partial_again.tree,
            &policy,
            None,
            Some(&partial_again.record),
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        assert!(!resolved.tree.has_conflict());
        assert_eq!(
            git_bytes(&resolved.tree, "conflicted").await,
            b"human resolved\n"
        );
        assert!(
            resolved.record.paths["conflicted"]
                .conflict_marker_len
                .is_none()
        );
        // Without owned original marker provenance, edited marker text cannot
        // silently become a resolved file under a guessed marker length.
        assert!(
            project_source(
                store,
                &changed_source,
                &conflict,
                &policy,
                None,
                None,
                ProjectionBudget::default()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("receipt")
        );
    });
}

#[test]
fn unsupported_names_targets_submodules_and_budget_fail_before_namespace_or_history_mutation() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = store.put_blob(b"UTF-8 Git projection").unwrap();
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        let heads = repo.loader().op_heads_store().get_op_heads().await.unwrap();
        for entries in [
            vec![(
                b"raw-\xff".as_slice(),
                put_file(&store, &stage.id, b"x", 0o644),
            )],
            vec![(
                b"link".as_slice(),
                EntryNode::Symlink {
                    target_hex: "ff".into(),
                },
            )],
            vec![(
                b".GIT".as_slice(),
                put_file(&store, &stage.id, b"reserved", 0o644),
            )],
        ] {
            let root = directory(&store, &stage.id, entries);
            assert!(
                project_source(
                    store.clone(),
                    &root,
                    &empty,
                    &policy,
                    None,
                    None,
                    ProjectionBudget::default()
                )
                .await
                .is_err()
            );
        }
        let source = directory(
            &store,
            &stage.id,
            vec![(b"file", put_file(&store, &stage.id, b"bounded", 0o644))],
        );
        let budget = ProjectionBudget {
            max_content_bytes: 2,
            ..ProjectionBudget::default()
        };
        assert!(
            project_source(store.clone(), &source, &empty, &policy, None, None, budget)
                .await
                .is_err()
        );
        let mut tree = MergedTreeBuilder::new(empty.clone());
        tree.set_or_remove(
            path("module"),
            Merge::resolved(Some(TreeValue::GitSubmodule(
                repo.store().root_commit_id().clone(),
            ))),
        );
        let submodule = tree.write_tree().await.unwrap();
        assert!(
            project_source(
                store,
                &source,
                &submodule,
                &policy,
                None,
                None,
                ProjectionBudget::default()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("submodule")
        );
        assert_eq!(
            repo.loader().op_heads_store().get_op_heads().await.unwrap(),
            heads
        );
        assert!(!dir.path().join("file").exists());
    });
}

#[test]
fn a_corrupt_cas_stream_returns_error_instead_of_jj_reader_panic() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = store.put_blob(b"verified stream").unwrap();
        let node = put_file(&store, &stage.id, b"valid bytes", 0o644);
        let EntryNode::File { object, .. } = &node else {
            unreachable!()
        };
        let source = directory(&store, &stage.id, vec![(b"file", node.clone())]);
        // Explicit corruption injection in this private fixture only.
        let blob_path = store.object_path(object).unwrap();
        std::fs::remove_file(&blob_path).unwrap();
        std::fs::write(blob_path, b"wrong bytes").unwrap();
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        assert!(
            project_source(
                store,
                &source,
                &empty,
                &policy,
                None,
                None,
                ProjectionBudget::default()
            )
            .await
            .is_err()
        );
    });
}
