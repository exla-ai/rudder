#![cfg(feature = "jj")]
mod common;

use common::{binding, commit, settings};
use jj_lib::{object_id::ObjectId, ref_name::WorkspaceName, repo::Repo, workspace::Workspace};
use rudder_vcs_bridge::{CommitIdentity, JjOperationId, seed::*};
use std::{fs, path::Path};

fn synthetic_internal(root: &Path) {
    fs::create_dir_all(root.join(".jj/repo/store/git/objects/aa")).unwrap();
    fs::create_dir_all(root.join(".jj/working_copy")).unwrap();
    fs::write(root.join(".jj/repo/store/git_target"), b"git").unwrap();
    fs::write(root.join(".jj/repo/store/git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    fs::write(root.join(".jj/repo/store/git/objects/aa/fixture"), b"object bytes").unwrap();
    fs::write(root.join(".jj/working_copy/type"), b"local").unwrap();
}

fn export(root: &Path, destination: &Path) -> anyhow::Result<MetadataTemplate> {
    prepare_internal_metadata_template(root, destination, commit(), binding().baseline_operation)
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            assert!(entry.file_type().unwrap().is_file());
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn internal_seed_reopens_original_commit_change_tree_and_operation_without_source_copy() {
    pollster::block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        fs::create_dir(&private).unwrap();
        let (_workspace, repo) = Workspace::init_internal_git(&settings(), &private).await.unwrap();
        let baseline = repo.store().get_commit(
            repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap(),
        ).unwrap();
        let identity = CommitIdentity::from_jj(&baseline);
        let operation = JjOperationId(repo.op_id().hex());
        let heads = repo.loader().op_heads_store().get_op_heads().await.unwrap();
        let git = private.join(".jj/repo/store/git");
        let original_config = fs::read(git.join("config")).unwrap();
        fs::create_dir_all(git.join("hooks")).unwrap();
        fs::write(git.join("hooks/post-checkout"), b"forbidden hook").unwrap();
        fs::write(private.join(".jj/repo/config.toml"), b"forbidden jj config").unwrap();
        fs::write(private.join(".jj/repo/rudder-prototype.log"), b"private log").unwrap();
        fs::create_dir(private.join("source-only-sentinel")).unwrap();
        fs::write(private.join("source-only-sentinel/never-copy"), b"not metadata").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("missing-source", private.join("source-symlink")).unwrap();

        let destination = temp.path().join("template");
        let manifest = prepare_internal_metadata_template(
            &private, &destination, identity.clone(), operation.clone(),
        ).unwrap();
        assert_eq!(manifest.baseline, identity);
        assert_eq!(manifest.operation, operation);
        assert_eq!(manifest.format_version, 1);
        assert_eq!(manifest.jj_version, "0.40.0");
        assert!(!manifest.objects.is_empty());
        assert!(!destination.join("metadata/.jj/repo/store/git").exists());
        assert!(!destination.join("metadata/source-only-sentinel").exists());
        assert!(!destination.join("metadata/.git/hooks").exists());
        assert!(!destination.join("metadata/.jj/repo/config.toml").exists());
        assert!(!destination.join("metadata/.jj/repo/rudder-prototype.log").exists());
        assert!(manifest.metadata.keys().all(|path| !path.starts_with(".jj/repo/store/git/")));
        assert_eq!(fs::read(destination.join("metadata/.jj/repo/store/git_target")).unwrap(), b"../../../.git");
        for (path, descriptor) in &manifest.objects {
            let bytes = fs::read(destination.join("objects").join(path)).unwrap();
            assert_eq!(bytes, fs::read(git.join("objects").join(path)).unwrap());
            assert_eq!(descriptor.size_bytes, bytes.len() as u64);
        }
        assert_eq!(fs::read(git.join("config")).unwrap(), original_config);
        assert_eq!(fs::read(private.join(".jj/repo/store/git_target")).unwrap(), b"git");
        assert_eq!(repo.loader().op_heads_store().get_op_heads().await.unwrap(), heads);

        // Independently attach COPIED objects to a copied test repository. This
        // verifies format interoperability, not production read-only custody.
        let restored = temp.path().join("restored");
        copy_directory(&destination.join("metadata"), &restored);
        copy_directory(&destination.join("objects"), &restored.join(".git/objects"));
        let loader = jj_lib::repo::RepoLoader::init_from_file_system(
            &settings(), &restored.join(".jj/repo"), &jj_lib::repo::StoreFactories::default(),
        ).unwrap();
        let loaded = loader.load_at_head().await.unwrap();
        assert_eq!(JjOperationId(loaded.op_id().hex()), operation);
        assert_eq!(CommitIdentity::from_jj(&loaded.store().get_commit(baseline.id()).unwrap()), identity);
        assert!(VerifiedMetadataTemplate::open(
            &destination, &destination.join("objects"), &manifest.identity().unwrap(),
        ).is_err(), "exporting bytes must not bypass the actual read-only mount requirement");
    });
}

#[test]
fn internal_and_colocated_inputs_emit_identical_existing_seed_format() {
    let temp = tempfile::tempdir().unwrap();
    let internal = temp.path().join("internal");
    synthetic_internal(&internal);
    let colocated = temp.path().join("colocated");
    copy_directory(&internal, &colocated);
    fs::rename(colocated.join(".jj/repo/store/git"), colocated.join(".git")).unwrap();
    fs::write(colocated.join(".jj/repo/store/git_target"), b"../../../.git").unwrap();
    let internal_manifest = export(&internal, &temp.path().join("internal-template")).unwrap();
    let colocated_manifest = prepare_metadata_template(
        &colocated, &temp.path().join("colocated-template"), commit(), binding().baseline_operation,
    ).unwrap();
    assert_eq!(internal_manifest, colocated_manifest);
    assert_eq!(internal_manifest.identity().unwrap(), colocated_manifest.identity().unwrap());
    assert!(prepare_metadata_template(
        &internal, &temp.path().join("wrong-api"), commit(), binding().baseline_operation,
    ).is_err());
}

#[test]
fn internal_export_refuses_external_or_alias_git_targets_before_destination_creation() {
    for target in [b"../git".as_slice(), b"git/".as_slice(), b"/outside/git".as_slice(), b"../../../.git".as_slice()] {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        synthetic_internal(&private);
        fs::write(private.join(".jj/repo/store/git_target"), target).unwrap();
        let destination = temp.path().join("rejected");
        assert!(export(&private, &destination).is_err());
        assert!(!destination.exists());
    }
}

#[test]
fn internal_export_refuses_alternates_common_directory_and_mixed_colocated_store() {
    for path in [".git", ".jj/repo/store/git/commondir", ".jj/repo/store/git/objects/info/alternates", ".jj/repo/store/git/objects/info/http-alternates"] {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        synthetic_internal(&private);
        let forbidden = private.join(path);
        fs::create_dir_all(forbidden.parent().unwrap()).unwrap();
        fs::write(forbidden, b"").unwrap();
        let destination = temp.path().join("rejected");
        assert!(export(&private, &destination).is_err());
        assert!(!destination.exists());
    }
}

#[cfg(unix)]
#[test]
fn internal_export_refuses_symlinked_hierarchy_target_and_dangling_alternates() {
    for part in ["", ".jj", ".jj/repo", ".jj/repo/store", ".jj/repo/store/git", ".jj/repo/store/git/objects", ".jj/repo/store/git_target"] {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        synthetic_internal(&private);
        let replaced = if part.is_empty() { private.clone() } else { private.join(part) };
        let held = temp.path().join("held-original");
        fs::rename(&replaced, &held).unwrap();
        std::os::unix::fs::symlink(&held, &replaced).unwrap();
        let destination = temp.path().join("rejected");
        assert!(export(&private, &destination).is_err());
        assert!(!destination.exists());
    }
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("private");
    synthetic_internal(&private);
    fs::create_dir_all(private.join(".jj/repo/store/git/objects/info")).unwrap();
    std::os::unix::fs::symlink("missing-alternates", private.join(".jj/repo/store/git/objects/info/alternates")).unwrap();
    let destination = temp.path().join("rejected");
    assert!(export(&private, &destination).is_err());
    assert!(!destination.exists());
}

#[cfg(unix)]
#[test]
fn failed_copy_keeps_partial_destination_without_a_success_manifest_or_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("private");
    synthetic_internal(&private);
    std::os::unix::fs::symlink("missing-metadata", private.join(".jj/repo/leak")).unwrap();
    let destination = temp.path().join("partial");
    assert!(export(&private, &destination).is_err());
    assert!(destination.is_dir());
    assert!(!destination.join("manifest.json").exists());
    fs::remove_file(private.join(".jj/repo/leak")).unwrap();
    assert!(export(&private, &destination).is_err());
    assert!(!destination.join("manifest.json").exists());
    assert_eq!(fs::read(private.join(".jj/repo/store/git_target")).unwrap(), b"git");
}

#[test]
fn internal_export_never_replaces_a_completed_seed_or_accepts_invalid_identity() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("private");
    synthetic_internal(&private);
    let destination = temp.path().join("template");
    export(&private, &destination).unwrap();
    let original = fs::read(destination.join("manifest.json")).unwrap();
    assert!(export(&private, &destination).is_err());
    assert_eq!(fs::read(destination.join("manifest.json")).unwrap(), original);
    let mut wrong = commit();
    wrong.commit.0 = "invalid".into();
    let invalid = temp.path().join("invalid");
    assert!(prepare_internal_metadata_template(
        &private, &invalid, wrong, binding().baseline_operation,
    ).is_err());
    assert!(!invalid.exists());
}
