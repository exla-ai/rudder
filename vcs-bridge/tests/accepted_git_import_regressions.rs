#![cfg(feature = "jj")]
mod common;

use gix::{ObjectId, objs::{Kind, Write as _}};
use jj_lib::{backend::CommitId, git_backend::GitBackend, object_id::ObjectId as _, repo::Repo, workspace::Workspace};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use rudder_vcs_bridge::{GitCommitId, accepted_git_import::*};
use std::{fs, path::Path, sync::Arc, time::Duration};

fn raw(repo: &gix::Repository, kind: Kind, bytes: &[u8]) -> ObjectId {
    repo.objects.write_stream(kind, bytes.len() as u64, &mut &bytes[..]).unwrap()
}
fn tree(repo: &gix::Repository, entries: &[(&str, &str, ObjectId)]) -> ObjectId {
    let mut bytes = Vec::new();
    for (mode, name, id) in entries {
        bytes.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        bytes.extend_from_slice(id.as_bytes());
    }
    raw(repo, Kind::Tree, &bytes)
}
fn commit(repo: &gix::Repository, tree: ObjectId, parents: &[ObjectId], message: &str) -> ObjectId {
    let mut bytes = format!("tree {tree}\n");
    for parent in parents { bytes.push_str(&format!("parent {parent}\n")); }
    bytes.push_str(&format!("author A <a@invalid.example> 1 +0000\ncommitter A <a@invalid.example> 1 +0000\n\n{message}\n"));
    raw(repo, Kind::Commit, bytes.as_bytes())
}
fn head(repo: &gix::Repository, id: ObjectId) {
    fs::write(repo.path().join("HEAD"), format!("{id}\n")).unwrap();
}
fn budget() -> FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(30)).unwrap() }

struct Fixture {
    _root: tempfile::TempDir,
    _workspace: Workspace,
    source: gix::Repository,
    target: Arc<jj_lib::repo::ReadonlyRepo>,
    head: ObjectId,
    parent: ObjectId,
    content: ObjectId,
    unrelated: ObjectId,
    closure: Vec<ObjectId>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = gix::init_bare(root.path().join("source.git")).unwrap();
        let content = raw(&source, Kind::Blob, b"tracked actual bytes\n");
        let link = raw(&source, Kind::Blob, b"tracked");
        let attributes = raw(&source, Kind::Blob, b"tracked export-ignore export-subst\n");
        let nested = tree(&source, &[("100755", "tool", content)]);
        let directory = tree(&source, &[("100644", ".gitattributes", attributes),
            ("40000", "dir", nested), ("120000", "link", link), ("100644", "tracked", content)]);
        let parent = commit(&source, directory, &[], "parent");
        let current = commit(&source, directory, &[parent], "original accepted bytes");
        head(&source, current);
        let unrelated = raw(&source, Kind::Blob, b"unreachable must not be copied");
        let target_path = root.path().join("target");
        fs::create_dir(&target_path).unwrap();
        let (workspace, target) = pollster::block_on(Workspace::init_internal_git(&common::settings(), &target_path)).unwrap();
        Self { _root: root, _workspace: workspace, source, target, head: current, parent, content, unrelated,
            closure: vec![current, parent, directory, nested, attributes, content, link] }
    }
    fn backend(&self) -> &GitBackend { self.target.store().backend_impl::<GitBackend>().unwrap() }
    fn expected(&self) -> GitCommitId { GitCommitId(self.head.to_string()) }
    fn run(&self, limits: GitImportLimits, original: &FinalizationBudget) -> anyhow::Result<ImportedGitHeadData> {
        import_expected_git_head(&self.source, self.backend(), &self.expected(), limits, original)
    }
}

#[test]
fn copies_exact_complete_reachable_history_and_modes_then_imports_original_commit() {
    let f = Fixture::new();
    let before = pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap();
    fs::create_dir_all(f.source.path().join("hooks")).unwrap();
    fs::write(f.source.path().join("hooks/post-checkout"), b"never execute or copy").unwrap();
    let original = budget();
    let result = f.run(GitImportLimits::default(), &original).unwrap();
    assert_eq!(result.head, f.expected());
    assert_eq!((result.objects, result.commits, result.trees, result.blobs), (7, 2, 2, 3));
    let destination = f.backend().git_repo();
    let mut bytes = 0;
    for id in &f.closure {
        let source = f.source.find_object(*id).unwrap();
        let copied = destination.find_object(*id).unwrap();
        assert_eq!(source.kind, copied.kind);
        assert_eq!(source.data, copied.data, "preserve original object preimages");
        bytes += source.data.len() as u64;
    }
    assert_eq!(result.bytes, bytes);
    assert!(destination.find_object(f.unrelated).is_err());
    assert!(!destination.path().join("hooks/post-checkout").exists());
    assert!(!destination.path().join("objects/info/alternates").exists());
    let imported = f.target.store().get_commit(&CommitId::new(f.head.as_bytes().to_vec())).unwrap();
    assert_eq!(imported.id().hex(), f.head.to_string());
    assert_eq!(imported.parent_ids(), &[CommitId::new(f.parent.as_bytes().to_vec())]);
    assert_eq!(pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap(), before);
    assert!(f.target.view().get_local_bookmark(jj_lib::ref_name::RefName::new(
        rudder_vcs_bridge::publication::INTEGRATION_BOOKMARK)).is_absent());
    assert_eq!(f.source.head().unwrap().id().unwrap().detach(), f.head);
    // Exact same immutable import is explicit and idempotent, never a new jj
    // operation or accepted-source result. The SAME original budget is used.
    assert_eq!(f.run(GitImportLimits::default(), &original).unwrap(), result);
}

#[test]
fn changed_or_wrong_expected_head_is_refused_before_any_target_object_write() {
    let f = Fixture::new();
    head(&f.source, f.parent);
    assert!(f.run(GitImportLimits::default(), &budget()).is_err());
    assert!(f.backend().git_repo().find_object(f.head).is_err());
    assert!(import_expected_git_head(&f.source, f.backend(), &GitCommitId("A".repeat(40)),
        GitImportLimits::default(), &budget()).is_err());
}

#[test]
fn object_edge_total_and_per_object_limits_and_original_cancellation_fail_closed() {
    let defaults = GitImportLimits::default();
    for limits in [GitImportLimits { max_objects: 2, ..defaults },
        GitImportLimits { max_edges: 1, ..defaults }, GitImportLimits { max_total_bytes: 1, ..defaults },
        GitImportLimits { max_object_bytes: 1, ..defaults }, GitImportLimits { max_objects: 0, ..defaults }] {
        let f = Fixture::new();
        assert!(f.run(limits, &budget()).is_err());
        assert!(f.backend().git_repo().find_object(f.head).is_err());
    }
    let f = Fixture::new();
    let original = budget();
    original.cancel();
    assert!(f.run(defaults, &original).is_err());
    assert!(f.backend().git_repo().find_object(f.head).is_err());
    assert!(f.run(defaults, &original).is_err(), "cancellation cannot start a fresh import window");
}

#[test]
fn gitlink_refusal_retains_partial_copied_objects_without_publishing_an_operation() {
    let f = Fixture::new();
    let directory = tree(&f.source, &[("160000", "submodule", f.parent)]);
    let current = commit(&f.source, directory, &[f.parent], "unsupported gitlink");
    head(&f.source, current);
    let before = pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap();
    assert!(import_expected_git_head(&f.source, f.backend(), &GitCommitId(current.to_string()),
        GitImportLimits::default(), &budget()).is_err());
    assert!(f.backend().git_repo().find_object(current).is_ok(), "error is not proof of no immutable effects");
    assert!(f.backend().git_repo().find_object(directory).is_err());
    assert_eq!(pollster::block_on(f.target.loader().op_heads_store().get_op_heads()).unwrap(), before);
}

#[test]
fn refuses_shallow_alternate_and_replacement_source_state_without_network_or_copy() {
    for marker in ["shallow", "objects/info/alternates", "objects/info/http-alternates"] {
        let f = Fixture::new();
        let path = f.source.path().join(marker);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
        assert!(f.run(GitImportLimits::default(), &budget()).is_err());
        assert!(f.backend().git_repo().find_object(f.head).is_err());
    }
    for packed in [false, true] {
        let f = Fixture::new();
        if packed {
            fs::write(f.source.path().join("packed-refs"),
                format!("# pack-refs with: sorted\n{} refs/replace/{}\n", f.parent, f.head)).unwrap();
        } else {
            fs::create_dir_all(f.source.path().join("refs/replace")).unwrap();
            fs::write(f.source.path().join("refs/replace").join(f.head.to_string()),
                format!("{}\n", f.parent)).unwrap();
        }
        let mut config = fs::read(f.source.path().join("config")).unwrap();
        // Pinned gix 0.81.0 replacement_objects_refs_prefix returns None for
        // true. Assert the actual map state instead of inferring Git semantics.
        config.extend_from_slice(b"\n[core]\nuseReplaceRefs = true\n");
        fs::write(f.source.path().join("config"), config).unwrap();
        let no_mapping = gix::open(f.source.path()).unwrap();
        assert!(no_mapping.objects.store_ref().replacements().next().is_none(),
            "this regression must exercise actual refs hidden from the enabled ODB map");
        for source in [&f.source, &no_mapping] {
            assert!(import_expected_git_head(source, f.backend(), &f.expected(),
                GitImportLimits::default(), &budget()).is_err());
            assert!(f.backend().git_repo().find_object(f.head).is_err());
        }
    }
}

#[test]
fn corrupt_object_preimage_and_malformed_commit_tree_and_kind_edges_are_refused() {
    let f = Fixture::new();
    let replacement = raw(&f.source, Kind::Blob, b"different corrupt preimage\n");
    fn loose(root: &Path, id: ObjectId) -> std::path::PathBuf {
        let hex = id.to_string(); root.join("objects").join(&hex[..2]).join(&hex[2..])
    }
    fs::remove_file(loose(f.source.path(), f.content)).unwrap();
    fs::copy(loose(f.source.path(), replacement), loose(f.source.path(), f.content)).unwrap();
    assert!(f.run(GitImportLimits::default(), &budget()).is_err());

    for case in 0..4 {
        let f = Fixture::new();
        let invalid = match case {
            0 => raw(&f.source, Kind::Commit, b"not a commit\n"),
            1 => commit(&f.source, f.content, &[], "tree points to blob"),
            2 => { let bad = raw(&f.source, Kind::Tree, b"not a tree\0"); commit(&f.source, bad, &[], "bad tree") },
            _ => { let duplicate = tree(&f.source, &[("100644", "same", f.content), ("100644", "same", f.content)]);
                commit(&f.source, duplicate, &[], "duplicate tree names") },
        };
        head(&f.source, invalid);
        assert!(import_expected_git_head(&f.source, f.backend(), &GitCommitId(invalid.to_string()),
            GitImportLimits::default(), &budget()).is_err());
    }
}

#[test]
fn refuses_colocated_or_redirected_target_and_target_alternates_before_copy() {
    let f = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let (_workspace, repo) = pollster::block_on(Workspace::init_colocated_git(&common::settings(), root.path())).unwrap();
    let colocated = repo.store().backend_impl::<GitBackend>().unwrap();
    assert!(import_expected_git_head(&f.source, colocated, &f.expected(), GitImportLimits::default(), &budget()).is_err());
    assert!(colocated.git_repo().find_object(f.head).is_err());

    let f = Fixture::new();
    let git = f.backend().git_repo_path();
    fs::create_dir_all(git.join("objects/info")).unwrap();
    fs::write(git.join("objects/info/alternates"), b"").unwrap();
    assert!(f.run(GitImportLimits::default(), &budget()).is_err());
    fs::remove_file(git.join("objects/info/alternates")).unwrap();
    fs::write(git.parent().unwrap().join("git_target"), b"../git").unwrap();
    assert!(f.run(GitImportLimits::default(), &budget()).is_err());
    assert!(f.backend().git_repo().find_object(f.head).is_err());
}


#[test]
fn packed_target_replacements_and_source_namespace_blinding_are_refused_before_copy() {
    let f = Fixture::new();
    fs::write(f.backend().git_repo_path().join("packed-refs"),
        format!("# pack-refs with: sorted\n{} refs/replace/{}\n", f.parent, f.head)).unwrap();
    assert!(f.run(GitImportLimits::default(), &budget()).is_err());
    assert!(f.backend().git_repo().find_object(f.head).is_err());

    let mut f = Fixture::new();
    fs::create_dir_all(f.source.path().join("refs/replace")).unwrap();
    fs::write(f.source.path().join("refs/replace").join(f.head.to_string()),
        format!("{}\n", f.parent)).unwrap();
    f.source.set_namespace("masked").unwrap();
    assert!(f.run(GitImportLimits::default(), &budget()).is_err(),
        "a selected namespace cannot hide actual repository replacement refs");
    assert!(f.backend().git_repo().find_object(f.head).is_err());
}
