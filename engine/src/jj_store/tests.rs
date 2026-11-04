use super::*;
use jj_lib::backend::TreeValue;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::{Repo as _, RepoLoader};
use jj_lib::rewrite::{merge_commit_trees, rebase_commit};

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", "Rudder test").unwrap();
    layer.set_value("user.email", "test@invalid.example").unwrap();
    config.add_layer(layer);
    UserSettings::from_config(config).unwrap()
}

fn temp() -> tempfile::TempDir {
    if Path::new("/workspace").is_dir() {
        tempfile::tempdir_in("/workspace").unwrap()
    } else {
        tempfile::tempdir().unwrap()
    }
}

fn with_file(repo: &Arc<ReadonlyRepo>, base: &MergedTree, path: &str, text: &str) -> MergedTree {
    let path = RepoPathBuf::from_internal_string(path).unwrap();
    let id = repo.store().write_file(&path, &mut text.as_bytes()).block_on().unwrap();
    let mut builder = MergedTreeBuilder::new(base.clone());
    builder.set_or_remove(
        path,
        Merge::normal(TreeValue::File { id, executable: false, copy_id: CopyId::placeholder() }),
    );
    builder.write_tree().block_on().unwrap()
}

fn read(tree: &MergedTree, path: &str) -> Option<String> {
    let path = RepoPathBuf::from_internal_string(path).unwrap();
    let value = tree.path_value(&path).block_on().unwrap();
    match value.into_resolved().unwrap()? {
        TreeValue::File { id, .. } => {
            let mut reader = tree.store().read_file(&path, &id).block_on().unwrap();
            let mut out = String::new();
            reader.read_to_string(&mut out).block_on().unwrap();
            Some(out)
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn jj_repo_on_rudder_store_commits_rebases_and_merges() {
    let dir = temp();
    let ws_root = dir.path().join("ws");
    let store_root = dir.path().join("cas");
    std::fs::create_dir(&ws_root).unwrap();
    let settings = settings();
    let (_ws, repo) = init_workspace(&settings, &ws_root, &store_root).unwrap();
    let type_file = ws_root.join(".jj/repo/store/type");
    assert_eq!(std::fs::read_to_string(type_file).unwrap(), BACKEND_NAME);
    assert!(repo.store().backend_impl::<RudderBackend>().is_some());

    // A: root -> a.txt
    let root = repo.store().root_commit();
    let empty = repo.store().empty_merged_tree();
    let tree_a = with_file(&repo, &empty, "a.txt", "one\n");
    let mut tx = repo.start_transaction();
    let a = tx.repo_mut().new_commit(vec![root.id().clone()], tree_a.clone()).set_description("A")
        .write().block_on().unwrap();
    // B: A -> edits a.txt; C: A -> adds dir/b.txt
    let tree_b = with_file(&repo, &tree_a, "a.txt", "two\n");
    let b = tx.repo_mut().new_commit(vec![a.id().clone()], tree_b).set_description("B")
        .write().block_on().unwrap();
    let tree_c = with_file(&repo, &tree_a, "dir/b.txt", "bee\n");
    let c = tx.repo_mut().new_commit(vec![a.id().clone()], tree_c).set_description("C")
        .write().block_on().unwrap();
    let repo = tx.commit("build").block_on().unwrap();

    // File ids are Rudder blob digests: the content is shared with snapshots.
    let backend = repo.store().backend_impl::<RudderBackend>().unwrap();
    let blob = backend.store().put_blob(b"one\n").unwrap();
    let a_path = RepoPathBuf::from_internal_string("a.txt").unwrap();
    let a_value = a.tree().path_value(&a_path).block_on().unwrap();
    match a_value.into_resolved().unwrap() {
        Some(TreeValue::File { id, .. }) => assert_eq!(id.hex(), blob.digest),
        other => panic!("unexpected {other:?}"),
    }
    // Commit ids are Rudder object digests too.
    assert!(backend.object_ref(&a.id().hex()).unwrap().is_some());

    // Rebase B onto C, then merge the rebased B with C's sibling A2.
    let mut tx = repo.start_transaction();
    let b2 = rebase_commit(tx.repo_mut(), b.clone(), vec![c.id().clone()]).block_on().unwrap();
    tx.repo_mut().rebase_descendants().block_on().unwrap();
    assert_eq!(read(&b2.tree(), "a.txt").as_deref(), Some("two\n"));
    assert_eq!(read(&b2.tree(), "dir/b.txt").as_deref(), Some("bee\n"));
    let tree_d = with_file(&repo, &tree_a, "d.txt", "dee\n");
    let d = tx.repo_mut().new_commit(vec![a.id().clone()], tree_d).set_description("D")
        .write().block_on().unwrap();
    let merged_tree = merge_commit_trees(tx.repo_mut(), &[b2.clone(), d.clone()]).block_on().unwrap();
    let merge = tx.repo_mut()
        .new_commit(vec![b2.id().clone(), d.id().clone()], merged_tree)
        .set_description("merge")
        .write().block_on().unwrap();
    let repo = tx.commit("rebase and merge").block_on().unwrap();
    assert!(!merge.has_conflict());

    // Reopen from disk through the store factories and walk the log.
    let loader = RepoLoader::init_from_file_system(&settings, &ws_root.join(".jj/repo"), &store_factories()).unwrap();
    let reopened = loader.load_at_head().block_on().unwrap();
    assert_eq!(reopened.op_id(), repo.op_id());
    assert!(reopened.view().heads().contains(merge.id()));
    let merge = reopened.store().get_commit(merge.id()).unwrap();
    for (path, text) in [("a.txt", "two\n"), ("dir/b.txt", "bee\n"), ("d.txt", "dee\n")] {
        assert_eq!(read(&merge.tree(), path).as_deref(), Some(text));
    }
    let mut log = Vec::new();
    let mut current = reopened.store().get_commit(b2.id()).unwrap();
    while current.id() != reopened.store().root_commit_id() {
        log.push(current.description().to_owned());
        current = reopened.store().get_commit(&current.parent_ids()[0]).unwrap();
    }
    assert_eq!(log, ["B", "C", "A"]);
    assert_eq!(merge.parent_ids(), [b2.id().clone(), d.id().clone()]);
}

#[test]
fn missing_objects_are_not_found_and_trees_round_trip() {
    let dir = temp();
    let backend = RudderBackend::new(LocalStore::open(dir.path().join("cas")).unwrap()).unwrap();
    let missing = backend.read_commit(&CommitId::new(vec![7; 32])).block_on();
    assert!(matches!(missing, Err(BackendError::ObjectNotFound { .. })));
    let empty = backend.read_tree(RepoPath::root(), backend.empty_tree_id()).block_on().unwrap();
    assert!(empty.is_empty());
    let link = backend.write_symlink(RepoPath::root(), "../target").block_on().unwrap();
    assert_eq!(backend.read_symlink(RepoPath::root(), &link).block_on().unwrap(), "../target");
}

#[test]
fn store_gc_refuses_to_sweep_jj_history() {
    let dir = temp();
    let ws_root = dir.path().join("ws");
    let store_root = dir.path().join("cas");
    std::fs::create_dir(&ws_root).unwrap();
    let (_ws, _repo) = init_workspace(&settings(), &ws_root, &store_root).unwrap();
    let store = LocalStore::open(&store_root).unwrap();
    let err = store.gc(std::time::Duration::ZERO).unwrap_err();
    assert!(err.to_string().contains("jj history"), "{err}");
}
