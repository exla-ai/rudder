mod common;
use common::*;
use rudder_engine::protocol::ObjectKind;
use rudder_vcs_bridge::{seed::*, *};
use std::fs;
use std::path::Path;

#[test]
fn raw_ids_conflict_terms_and_live_vs_sealed_types_are_distinct() {
    let mut id = GitCommitId("f".repeat(64));
    assert!(id.validate().is_err());
    id.0 = "F".repeat(40);
    assert!(id.validate().is_err());
    let mut merged = commit().tree;
    merged
        .terms
        .extend([GitTreeId("a".repeat(40)), GitTreeId("b".repeat(40))]);
    merged.labels = vec!["left".into(), "base".into(), "right".into()];
    merged.validate().unwrap();
    merged.labels.pop();
    assert!(merged.validate().is_err());
    let binding = binding();
    let mut observed = observation(&binding);
    observed.validate(&binding).unwrap();
    assert!(
        serde_json::from_value::<SealedSourceProof>(serde_json::to_value(&observed).unwrap())
            .is_err()
    );
    observed.view_id = "sibling".into();
    assert!(observed.validate(&binding).is_err());
    let mut sealed = proof(binding, commit().tree);
    let before = sealed.identity().unwrap();
    sealed.binding.run_epoch += 1;
    assert_ne!(sealed.identity().unwrap(), before);
    sealed.source.kind = ObjectKind::Directory;
    assert!(sealed.validate().is_err());
}

#[test]
fn real_vfs_observation_token_preserves_namespace_nonce_and_exclusion_scope() {
    use rudder_engine::vfs::{DirtyCursor, ObservationConsistency, VfsObservation};
    let binding = binding();
    let cursor = DirtyCursor {
        view_id: binding.view_id.clone(),
        owner: binding.owner.clone(),
        namespace: "00aabd55-11fc-4a70-9d62-98407743e331".into(),
        generation: 2,
    };
    let mut observed = VfsObservation {
        format_version: 1,
        view_id: binding.view_id.clone(),
        owner: binding.owner.clone(),
        begin: cursor.clone(),
        end: cursor,
        directory: reference(ObjectKind::Directory, "visible-source-directory"),
        exclusion_policy_digest: "a".repeat(64),
        request_nonce: "69d882b5-c581-4894-ad31-ae820ad61561".into(),
        pin_id: "owned-pin".into(),
        consistency: ObservationConsistency::LiveBestEffort,
    };
    let token = LiveObservation::from_vfs(&observed, &binding).unwrap();
    assert_eq!(token.request_nonce, observed.request_nonce);
    assert_eq!(token.cursor_namespace, observed.begin.namespace);
    assert_eq!(token.projection_policy, binding.projection_policy);
    assert_ne!(
        token.exclusion_policy_digest,
        token.projection_policy.digest
    );
    observed.end.namespace = "69d882b5-c581-4894-ad31-ae820ad61561".into();
    assert!(LiveObservation::from_vfs(&observed, &binding).is_err());
}

fn prepared_fixture(root: &Path) -> MetadataTemplate {
    fs::create_dir_all(root.join("private/.git/objects/aa")).unwrap();
    fs::create_dir_all(root.join("private/.git/hooks")).unwrap();
    fs::create_dir_all(root.join("private/.jj/repo/store")).unwrap();
    fs::create_dir_all(root.join("private/.jj/working_copy")).unwrap();
    fs::write(root.join("private/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        root.join("private/.git/config"),
        "[credential]\nhelper = forbidden-fixture-sentinel\n",
    )
    .unwrap();
    fs::write(
        root.join("private/.git/hooks/post-checkout"),
        "forbidden-hook",
    )
    .unwrap();
    fs::write(
        root.join("private/.git/objects/aa/fixture"),
        "immutable object fixture",
    )
    .unwrap();
    fs::write(
        root.join("private/.jj/repo/config.toml"),
        "forbidden-jj-config",
    )
    .unwrap();
    fs::write(
        root.join("private/.jj/repo/store/git_target"),
        "../../../.git",
    )
    .unwrap();
    fs::write(root.join("private/.jj/working_copy/type"), "local").unwrap();
    fs::create_dir_all(root.join("private/deps/large-tree")).unwrap();
    fs::write(
        root.join("private/deps/large-tree/not-metadata"),
        "source/deps must not be walked or copied",
    )
    .unwrap();
    prepare_metadata_template(
        &root.join("private"),
        &root.join("template"),
        commit(),
        binding().baseline_operation,
    )
    .unwrap()
}

#[test]
fn preparation_strips_executable_configuration_and_never_copies_source() {
    let root = tempfile::tempdir().unwrap();
    let manifest = prepared_fixture(root.path());
    let dir = root.path().join("template/metadata");
    assert!(!dir.join("deps").exists());
    assert!(!dir.join(".git/hooks").exists());
    assert!(!dir.join(".jj/repo/config.toml").exists());
    assert!(
        !fs::read_to_string(dir.join(".git/config"))
            .unwrap()
            .contains("forbidden")
    );
    assert_eq!(
        fs::read_to_string(dir.join(".jj/repo/store/git_target")).unwrap(),
        "../../../.git"
    );
    assert!(
        VerifiedMetadataTemplate::open(
            &root.path().join("template"),
            &root.path().join("template/objects"),
            &manifest.identity().unwrap()
        )
        .is_err(),
        "ordinary writable filesystem is not a read-only object mount"
    );
    assert!(
        prepare_metadata_template(
            &root.path().join("private"),
            &root.path().join("template"),
            commit(),
            binding().baseline_operation
        )
        .is_err(),
        "never overwrite an existing artifact"
    );
}

#[test]
fn malformed_seed_paths_symlinks_and_external_alternates_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let mut manifest = prepared_fixture(root.path());
    let node = manifest.metadata.values().next().unwrap().clone();
    manifest.metadata.insert("../outside".into(), node);
    assert!(manifest.validate().is_err());
    fs::create_dir_all(root.path().join("private/.git/objects/info")).unwrap();
    fs::write(
        root.path().join("private/.git/objects/info/alternates"),
        "/engine-object-store\n",
    )
    .unwrap();
    assert!(
        prepare_metadata_template(
            &root.path().join("private"),
            &root.path().join("rejected"),
            commit(),
            binding().baseline_operation
        )
        .is_err()
    );
    #[cfg(unix)]
    {
        fs::remove_file(root.path().join("private/.git/objects/info/alternates")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.path().join("private/.jj/repo/leak"))
            .unwrap();
        assert!(
            prepare_metadata_template(
                &root.path().join("private"),
                &root.path().join("symlink-rejected"),
                commit(),
                binding().baseline_operation
            )
            .is_err()
        );
        assert!(!root.path().join("symlink-rejected/manifest.json").exists());
        fs::remove_file(root.path().join("private/.jj/repo/leak")).unwrap();
        fs::rename(
            root.path().join("private/.git"),
            root.path().join("outside.git"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            root.path().join("outside.git"),
            root.path().join("private/.git"),
        )
        .unwrap();
        assert!(
            prepare_metadata_template(
                &root.path().join("private"),
                &root.path().join("external-git-rejected"),
                commit(),
                binding().baseline_operation
            )
            .is_err()
        );
        assert!(!root.path().join("external-git-rejected").exists());
    }
}

#[cfg(feature = "local-development")]
#[test]
fn unlisted_git_objects_cannot_hide_in_an_otherwise_valid_seed_manifest() {
    let root = tempfile::tempdir().unwrap();
    let manifest = prepared_fixture(root.path());
    fs::write(
        root.path().join("template/objects/aa/unlisted"),
        "unlisted history",
    )
    .unwrap();
    assert!(
        VerifiedMetadataTemplate::open_development_integrity_only(
            &root.path().join("template"),
            &root.path().join("template/objects"),
            &manifest.identity().unwrap()
        )
        .is_err()
    );
}

#[cfg(feature = "local-development")]
#[test]
fn private_forks_diverge_without_touching_seed_and_corruption_never_becomes_ready() {
    let root = tempfile::tempdir().unwrap();
    let manifest = prepared_fixture(root.path());
    let template = VerifiedMetadataTemplate::open_development_integrity_only(
        &root.path().join("template"),
        &root.path().join("template/objects"),
        &manifest.identity().unwrap(),
    )
    .unwrap();
    let mut first = binding();
    first.attempt_id = "a".into();
    first.view_id = "view-a".into();
    let mut second = binding();
    second.attempt_id = "b".into();
    second.view_id = "view-b".into();
    let a = fork_private_metadata(&template, &root.path().join("a"), first, None).unwrap();
    let b = fork_private_metadata(&template, &root.path().join("b"), second, None).unwrap();
    assert!(!a.kernel_readonly_objects && !b.kernel_readonly_objects);
    fs::write(root.path().join("a/.git/HEAD"), "private a\n").unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("b/.git/HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
    assert!(!root.path().join("a/deps").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            fs::metadata(root.path().join("a/.git/config"))
                .unwrap()
                .ino(),
            fs::metadata(root.path().join("b/.git/config"))
                .unwrap()
                .ino()
        );
    }
    fs::write(root.path().join("template/metadata/.git/HEAD"), "corrupted").unwrap();
    assert!(
        fork_private_metadata(&template, &root.path().join("corrupt"), binding(), None).is_err()
    );
    assert!(
        !root
            .path()
            .join("corrupt/rudder-metadata-ready.json")
            .exists()
    );
}
