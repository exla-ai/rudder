use rudder_engine::build::*;
use rudder_engine::protocol::{ObjectKind, ObjectRef, FORMAT_VERSION};
use rudder_engine::store::{
    CheckpointV2Manifest, DirectoryEntry, DirectoryManifest, EntryNode, SourceManifest,
};
use rudder_engine::store::{EnvironmentManifest, LocalStore};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

fn fixture() -> (tempfile::TempDir, BuildIndex, PreparationSpec, ObjectRef) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path().join("cas")).unwrap();
    let source = dir.path().join("toolchain");
    std::fs::create_dir_all(source.join("tools")).unwrap();
    std::fs::write(source.join("tools/compiler"), b"pinned compiler fixture\n").unwrap();
    let directory = store
        .import_tree(
            &source,
            rudder_engine::protocol::CheckpointConsistency::Quiesced,
        )
        .unwrap();
    let compiler = store.put_blob(b"pinned compiler fixture\n").unwrap();
    let lock = store.put_blob(b"lockfile-v1").unwrap();
    let spec = PreparationSpec {
        format_version: FORMAT_VERSION,
        platform: PlatformIdentity {
            os: "linux".into(),
            architecture: "x86_64".into(),
            abi: "test-abi".into(),
            properties: BTreeMap::new(),
        },
        base_runtime: directory.clone(),
        installer_inputs: [("package-lock.json".into(), lock)].into_iter().collect(),
        installer_commands: vec![vec!["fixed-installer".into(), "--frozen-lockfile".into()]],
        installer_environment: BTreeMap::new(),
        toolchains: [(
            "compiler".into(),
            ToolchainIdentity {
                version: "1".into(),
                closure: directory.clone(),
                executable: compiler,
                executable_path: "tools/compiler".into(),
                configuration: BTreeMap::new(),
            },
        )]
        .into_iter()
        .collect(),
        policy_version: "prepare-v1".into(),
        effects: Default::default(),
    };
    let index = BuildIndex::open(dir.path().join("index"), store).unwrap();
    (dir, index, spec, directory)
}

#[test]
fn prepared_root_is_retained_reopenable_and_source_independent() {
    let (dir, index, spec, directory) = fixture();
    let prepared = index.publish_prepared(&spec, &directory).unwrap();
    let expected = spec.key(index.store()).unwrap();
    index
        .store()
        .put_blob(b"edited application source is not an installer input")
        .unwrap();
    assert_eq!(spec.key(index.store()).unwrap(), expected);
    index.store().gc(Duration::ZERO).unwrap();
    assert_eq!(index.prepared(&spec).unwrap(), Some(prepared.clone()));
    let compiler = &spec.toolchains["compiler"].executable;
    assert_eq!(
        index.store().read_object(compiler).unwrap(),
        b"pinned compiler fixture\n"
    );
    let environment: EnvironmentManifest =
        serde_json::from_slice(&index.store().read_object(&prepared.root).unwrap()).unwrap();
    assert_eq!(environment.directory, directory);
    assert_eq!(
        index.prepared_root(&prepared.root).unwrap(),
        Some(prepared.clone())
    );
    drop(index);
    let reopened = BuildIndex::open(
        dir.path().join("index"),
        LocalStore::open(dir.path().join("cas")).unwrap(),
    )
    .unwrap();
    assert_eq!(reopened.prepared(&spec).unwrap(), Some(prepared));
}

#[test]
fn preparation_layouts_have_distinct_keys_roots_and_retained_index_authority() {
    let (dir, index, spec, directory) = fixture();
    let old = index.publish_prepared(&spec, &directory).unwrap();
    let new = index
        .publish_prepared_with_layout(&spec, &directory, PreparedContentLayout::PagedChunkedV2)
        .unwrap();
    assert_ne!(old.key, new.key);
    assert_ne!(old.root, new.root);
    assert_eq!(index.prepared(&spec).unwrap(), Some(old.clone()));
    assert_eq!(
        index
            .prepared_with_layout(&spec, PreparedContentLayout::PagedChunkedV2)
            .unwrap(),
        Some(new.clone())
    );
    let mut environment =
        rudder_engine::snapshot::decode_environment(&index.store().read_object(&new.root).unwrap())
            .unwrap();
    assert_eq!(environment.directory, directory);
    assert_eq!(index.preparation_spec(&new).unwrap(), spec);
    assert_eq!(index.prepared_root(&new.root).unwrap(), Some(new.clone()));
    environment
        .provenance
        .insert("policy_version".into(), "forged-v2-root".into());
    let lease = index
        .store()
        .begin_staging(Duration::from_secs(60))
        .unwrap();
    let forged = rudder_engine::snapshot::stage(
        index.store(),
        &lease.id,
        ObjectKind::EnvironmentRoot,
        &environment,
    )
    .unwrap();
    assert!(
        index.prepared_root(&forged).is_err(),
        "provenance cannot impersonate an indexed v2 root"
    );
    index.store().abandon_staging(&lease.id).unwrap();
    index.store().gc(Duration::ZERO).unwrap();
    drop(index);
    let reopened = BuildIndex::open(
        dir.path().join("index"),
        LocalStore::open(dir.path().join("cas")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        reopened.prepared_root(&new.root).unwrap(),
        Some(new.clone())
    );
    assert_eq!(reopened.preparation_spec(&new).unwrap(), spec);
    assert_eq!(reopened.prepared(&spec).unwrap(), Some(old));
}

#[test]
fn chunked_compiler_identity_is_checked_through_paged_paths() {
    use rudder_engine::{
        content::{self, StagedContentStore},
        directory::{
            DirectoryEntry as PagedEntry, EntryNode as PagedNode, LocalDirectoryStore,
            PagedDirectory,
        },
    };
    let (_dir, index, mut spec, _) = fixture();
    let store = index.store();
    let lease = store.begin_staging(Duration::from_secs(60)).unwrap();
    let bytes = vec![b'c'; content::CHUNK_BYTES + 19];
    let file = content::import_file(
        &StagedContentStore::new(store, &lease.id).unwrap(),
        &mut bytes.as_slice(),
    )
    .unwrap();
    assert_eq!(file.kind, ObjectKind::File);
    let adapter = LocalDirectoryStore::new(store, &lease.id);
    let tools = PagedDirectory::new(&adapter)
        .build(
            0o755,
            vec![PagedEntry::new(
                b"compiler",
                PagedNode::File {
                    object: file.clone(),
                    logical_size: bytes.len() as u64,
                    mode: 0o755,
                },
            )
            .unwrap()],
        )
        .unwrap();
    let directory = PagedDirectory::new(&adapter)
        .build(
            0o755,
            vec![PagedEntry::new(b"tools", PagedNode::Directory { object: tools }).unwrap()],
        )
        .unwrap();
    store
        .publish(&lease.id, "chunked-tool-fixture", &directory)
        .unwrap();
    spec.base_runtime = directory.clone();
    let tool = spec.toolchains.get_mut("compiler").unwrap();
    tool.closure = directory.clone();
    tool.executable = file.clone();
    let prepared = index
        .publish_prepared_with_layout(&spec, &directory, PreparedContentLayout::PagedChunkedV2)
        .unwrap();
    assert_eq!(
        index.preparation_spec(&prepared).unwrap().toolchains["compiler"].executable,
        file
    );
    spec.toolchains.get_mut("compiler").unwrap().executable =
        store.put_blob(b"wrong compiler").unwrap();
    assert!(index
        .publish_prepared_with_layout(&spec, &directory, PreparedContentLayout::PagedChunkedV2)
        .is_err());
    assert!(index
        .prepared_with_layout(&spec, PreparedContentLayout::PagedChunkedV2)
        .unwrap()
        .is_none());
}

#[test]
fn ancestry_free_preparation_rejects_history_inputs_before_index_publication() {
    let (_dir, index, mut spec, directory) = fixture();
    let history = index
        .store()
        .put_source(&SourceManifest {
            format_version: 1,
            consistency: rudder_engine::protocol::CheckpointConsistency::Quiesced,
            directory: directory.clone(),
            parents: vec![],
        })
        .unwrap();
    spec.installer_inputs.insert("history.json".into(), history);
    assert!(index
        .publish_prepared_with_layout(&spec, &directory, PreparedContentLayout::PagedChunkedV2)
        .is_err());
    assert!(index.store().retention().unwrap().staging.is_empty());
    assert!(index.store().retention().unwrap().pins.is_empty());
}

#[test]
fn environment_provenance_is_not_preparation_authority() {
    let (_dir, index, spec, directory) = fixture();
    let prepared = index.publish_prepared(&spec, &directory).unwrap();
    let mut environment: EnvironmentManifest =
        serde_json::from_slice(&index.store().read_object(&prepared.root).unwrap()).unwrap();
    environment
        .provenance
        .insert("policy_version".into(), "forged-root".into());
    let forged = index.store().put_environment(&environment).unwrap();
    assert!(index.prepared_root(&forged).is_err());
    environment
        .provenance
        .insert("preparation_key".into(), "a".repeat(64));
    let unindexed = index.store().put_environment(&environment).unwrap();
    assert!(index.prepared_root(&unindexed).unwrap().is_none());
    environment.provenance.clear();
    let foreign = index.store().put_environment(&environment).unwrap();
    assert!(index.prepared_root(&foreign).unwrap().is_none());
}

#[test]
fn installer_lock_runtime_compiler_platform_and_policy_changes_miss_separately() {
    let (_dir, index, spec, directory) = fixture();
    index.publish_prepared(&spec, &directory).unwrap();
    for changed in [
        "lock",
        "compiler",
        "runtime",
        "platform",
        "policy",
        "installer",
        "configuration",
    ] {
        let mut next = spec.clone();
        match changed {
            "lock" => {
                next.installer_inputs.insert(
                    "package-lock.json".into(),
                    index.store().put_blob(b"lock-v2").unwrap(),
                );
            }
            "compiler" => {
                next.toolchains.get_mut("compiler").unwrap().version = "2".into();
            }
            "runtime" => {
                next.base_runtime = index
                    .store()
                    .put_directory(rudder_engine::store::DirectoryManifest {
                        format_version: 1,
                        mode: 0o755,
                        entries: vec![],
                    })
                    .unwrap();
            }
            "platform" => next.platform.abi = "different-abi".into(),
            "policy" => next.policy_version = "prepare-v2".into(),
            "installer" => next.installer_commands[0].push("--different-option".into()),
            _ => {
                next.toolchains
                    .get_mut("compiler")
                    .unwrap()
                    .configuration
                    .insert("native-package-option".into(), "v2".into());
            }
        }
        assert!(
            index.prepared(&next).unwrap().is_none(),
            "{changed} must invalidate preparation"
        );
    }
}

#[test]
fn preparation_misses_coalesce_across_independent_connections() {
    let (dir, index, spec, directory) = fixture();
    let counter = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let root = dir.path().to_owned();
            let spec = spec.clone();
            let directory = directory.clone();
            let counter = counter.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let index = BuildIndex::open(
                    root.join("index"),
                    LocalStore::open(root.join("cas")).unwrap(),
                )
                .unwrap();
                barrier.wait();
                index
                    .prepare_once(&spec, |_| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(40));
                        Ok(directory)
                    })
                    .unwrap()
            })
        })
        .collect();
    let roots: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(roots[0], roots[1]);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(index.prepared(&spec).unwrap(), Some(roots[0].clone()));
}

#[test]
fn failed_prepare_or_wrong_compiler_never_enters_index() {
    let (_dir, index, spec, directory) = fixture();
    assert!(index
        .prepare_once(&spec, |_| anyhow::bail!("installer failed"))
        .is_err());
    assert!(index.prepared(&spec).unwrap().is_none());
    let mut invalid = spec.clone();
    invalid.toolchains.get_mut("compiler").unwrap().executable =
        index.store().put_blob(b"another compiler").unwrap();
    assert!(index.publish_prepared(&invalid, &directory).is_err());
    assert!(index.prepared(&invalid).unwrap().is_none());
}

#[test]
fn preparation_retention_begins_before_callback_and_lost_lease_cannot_publish() {
    let (_dir, index, spec, directory) = fixture();
    let prepared = index
        .prepare_once(&spec, |store| {
            assert!(store.gc(Duration::ZERO)?.blocked_by_staging);
            assert!(store.read_directory(&directory).is_ok());
            Ok(directory.clone())
        })
        .unwrap();
    assert_eq!(index.prepared(&spec).unwrap(), Some(prepared));
    assert!(index.store().retention().unwrap().staging.is_empty());

    let mut next = spec.clone();
    next.policy_version = "prepare-v2".into();
    let result = index.prepare_once(&next, |store| {
        let leases = store.retention()?.staging;
        assert_eq!(leases.len(), 1);
        // Model a lease which ceased to authorize this attempt. Publication
        // must fail instead of silently acquiring new retention afterward.
        store.abandon_staging(leases.keys().next().unwrap())?;
        Ok(directory.clone())
    });
    assert!(result.is_err());
    assert!(index.prepared(&next).unwrap().is_none());
}

#[test]
fn pin_precedes_index_ack_and_failed_sql_publication_keeps_conservative_root() {
    let (dir, index, spec, directory) = fixture();
    let db = rusqlite::Connection::open(dir.path().join("index/index.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_index BEFORE INSERT ON build_index BEGIN SELECT RAISE(ABORT,'simulated index failure'); END;").unwrap();
    assert!(index.publish_prepared(&spec, &directory).is_err());
    assert!(index.prepared(&spec).unwrap().is_none());
    assert_eq!(index.store().retention().unwrap().pins.len(), 1);
    index.store().gc(Duration::ZERO).unwrap();
    assert!(
        index.store().read_directory(&directory).is_ok(),
        "index failure must not release already durable data"
    );
    db.execute_batch("DROP TRIGGER fail_index").unwrap();
    let result = index.publish_prepared(&spec, &directory).unwrap();
    assert_eq!(index.prepared(&spec).unwrap(), Some(result));
}

#[test]
fn profiles_are_explicit_tracked_config_and_cannot_self_assert_shell_hermeticity() {
    let text = include_str!("../../examples/build-profile/rudder.toml");
    let profile = BuildProfile::from_toml(text).unwrap();
    assert_eq!(profile.targets.len(), 2);
    assert!(BuildProfile::from_toml(&("hermetic = true\n".to_owned() + text)).is_err());
    assert!(BuildProfile::from_toml(&text.replace(
        "SOURCE_DATE_EPOCH = \"0\"",
        "AWS_SECRET_ACCESS_KEY = \"not-a-compiler-input\""
    ))
    .is_err());
    let mut effectful = profile.clone();
    effectful.policy.effects.insert(BuildEffect::Deployment);
    assert!(!effectful.policy.cacheable());
    let cache = RemoteCacheConfig {
        cas_address: "grpc://127.0.0.1:8980".into(),
        action_cache_address: "grpc://127.0.0.1:8980".into(),
        instance_name: "tenant/profile".into(),
        tls: false,
    };
    assert!(cache.buckconfig().unwrap().contains("[buck2_re_client]"));
    assert!(cache
        .bazel_flags()
        .unwrap()
        .contains(&"--remote_cache=grpc://127.0.0.1:8980".to_owned()));
    let mut injection = cache.clone();
    injection.instance_name = "bad\n[project]".into();
    assert!(injection.validate().is_err());
    assert_ne!(
        ObjectRef::from_bytes(ObjectKind::Blob, b"protobuf action bytes").kind,
        ObjectKind::ActionKey,
        "Rudder metadata is not an REAPI Action protobuf"
    );
}

fn checkpoint(
    store: &LocalStore,
    source: &ObjectRef,
    environment: &ObjectRef,
    view: &ObjectRef,
    profile: &rudder_engine::workspace::WorkspaceProfile,
) -> ObjectRef {
    let profile = store
        .put_blob(&rudder_engine::protocol::canonical_json(profile).unwrap())
        .unwrap();
    store
        .put_checkpoint_v2(&CheckpointV2Manifest {
            format_version: 2,
            source: source.clone(),
            environment: Some(environment.clone()),
            parents: vec![],
            consistency: rudder_engine::protocol::CheckpointConsistency::Quiesced,
            durability: rudder_engine::protocol::Durability::Local,
            knowledge_cursor: 0,
            view_root: view.clone(),
            profile,
        })
        .unwrap()
}
fn source_from(store: &LocalStore, directory: &ObjectRef) -> ObjectRef {
    store
        .put_source(&SourceManifest {
            format_version: 1,
            directory: directory.clone(),
            parents: vec![],
            consistency: rudder_engine::protocol::CheckpointConsistency::Quiesced,
        })
        .unwrap()
}

#[test]
fn workspace_checkpoint_dirty_prepared_nodes_missing_controls_and_fake_source_are_verified() {
    let (dir, index, spec, tools) = fixture();
    let prepared = index.publish_prepared(&spec, &tools).unwrap();
    let store = index.store();
    let source_file = store.put_blob(b"pub fn answer()->u32{42}").unwrap();
    let source_dir = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![DirectoryEntry::new(
                b"main.rs",
                EntryNode::File {
                    object: source_file,
                    mode: 0o644,
                },
            )
            .unwrap()],
        })
        .unwrap();
    let source = source_from(store, &source_dir);
    let mut view = store.read_directory(&source_dir).unwrap();
    view.entries
        .extend(store.read_directory(&tools).unwrap().entries);
    let view = store.put_directory(view).unwrap();
    let profile = rudder_engine::workspace::WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"tools".to_vec()], vec![b"node_modules".to_vec()]],
        scratch: vec![vec![b"scratch".to_vec()]],
        vcs_local: vec![],
    };
    let clean = checkpoint(store, &source, &prepared.root, &view, &profile);
    let control = rudder_engine::workspace::build_input_view(store, &clean).unwrap();
    assert!(
        !control.dirty_prepared(),
        "a missing path in both base and view remains clean"
    );
    let changed = store.put_blob(b"changed compiler bytes").unwrap();
    let changed_view = store
        .update_path(
            &view,
            &[b"tools".to_vec(), b"compiler".to_vec()],
            Some(
                DirectoryEntry::new(
                    b"compiler",
                    EntryNode::File {
                        object: changed,
                        mode: 0o644,
                    },
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let deleted_view = store
        .update_path(&view, &[b"tools".to_vec()], None)
        .unwrap();
    let modules = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![],
        })
        .unwrap();
    let added_view = store
        .update_path(
            &view,
            &[b"node_modules".to_vec()],
            Some(
                DirectoryEntry::new(b"node_modules", EntryNode::Directory { object: modules })
                    .unwrap(),
            ),
        )
        .unwrap();
    for (name, dirty) in [
        ("compiler", changed_view),
        ("deleted", deleted_view),
        ("node_modules", added_view),
    ] {
        let cp = checkpoint(store, &source, &prepared.root, &dirty, &profile);
        let input = rudder_engine::workspace::build_input_view(store, &cp).unwrap();
        assert!(
            input.dirty_prepared(),
            "{name} must change the prepared fingerprint"
        );
        #[cfg(target_os = "linux")]
        {
            let adapter = Buck2Adapter::new("/bin/true").unwrap();
            let profile =
                BuildProfile::from_toml(include_str!("../../examples/build-profile/rudder.toml"))
                    .unwrap();
            let destination = dir.path().join(format!("denied-{name}"));
            let error = adapter
                .build(&index, &profile, &input, &prepared, &destination)
                .unwrap_err()
                .to_string();
            assert!(error.contains("dirty prepared"));
            assert!(!destination.exists());
        }
    }
    let fake_dir = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![],
        })
        .unwrap();
    let fake = source_from(store, &fake_dir);
    let forged = checkpoint(store, &fake, &prepared.root, &view, &profile);
    assert!(
        rudder_engine::workspace::build_input_view(store, &forged).is_err(),
        "a source-only ref cannot hide full-view changes"
    );
    let scratch = store.put_blob(b"private scratch").unwrap();
    let scratch_view = store
        .update_path(
            &view,
            &[b"scratch".to_vec()],
            Some(
                DirectoryEntry::new(
                    b"scratch",
                    EntryNode::File {
                        object: scratch,
                        mode: 0o600,
                    },
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let scratch_cp = checkpoint(store, &source, &prepared.root, &scratch_view, &profile);
    assert!(
        !rudder_engine::workspace::build_input_view(store, &scratch_cp)
            .unwrap()
            .dirty_prepared()
    );
}

#[test]
fn source_ancestor_modes_do_not_falsely_dirty_nested_prepared_partition() {
    let (_dir, index, _spec, _tools) = fixture();
    let store = index.store();
    let modules = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![],
        })
        .unwrap();
    let ancestor = |mode| {
        store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode,
                entries: vec![DirectoryEntry::new(
                    b"node_modules",
                    EntryNode::Directory {
                        object: modules.clone(),
                    },
                )
                .unwrap()],
            })
            .unwrap()
    };
    let base = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![DirectoryEntry::new(
                b"deps",
                EntryNode::Directory {
                    object: ancestor(0o755),
                },
            )
            .unwrap()],
        })
        .unwrap();
    let environment = store
        .put_environment(&EnvironmentManifest {
            format_version: 1,
            directory: base,
            platform: [("os".into(), "linux".into())].into_iter().collect(),
            installer_inputs: vec![],
            provenance: Default::default(),
        })
        .unwrap();
    let view = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![DirectoryEntry::new(
                b"deps",
                EntryNode::Directory {
                    object: ancestor(0o700),
                },
            )
            .unwrap()],
        })
        .unwrap();
    let empty = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o700,
            entries: vec![],
        })
        .unwrap();
    let source_dir = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![
                DirectoryEntry::new(b"deps", EntryNode::Directory { object: empty }).unwrap(),
            ],
        })
        .unwrap();
    let source = source_from(store, &source_dir);
    let profile = rudder_engine::workspace::WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"deps".to_vec(), b"node_modules".to_vec()]],
        scratch: vec![],
        vcs_local: vec![],
    };
    let cp = checkpoint(store, &source, &environment, &view, &profile);
    assert!(!rudder_engine::workspace::build_input_view(store, &cp)
        .unwrap()
        .dirty_prepared());
}

#[test]
fn v3_build_inputs_validate_and_key_hardlink_identity_separately_from_file_bytes() {
    use rudder_engine::{
        protocol::{canonical_json, CheckpointConsistency, Durability},
        store::CheckpointV3Manifest,
        vfs::{HardlinkGroup, HardlinkTopology},
    };
    let (_dir, index, spec, tools) = fixture();
    let prepared = index.publish_prepared(&spec, &tools).unwrap();
    let store = index.store();
    let bytes = store.put_blob(b"equal content, separate identity").unwrap();
    let mut entries = vec![];
    for name in [b"a", b"b"] {
        entries.push(
            DirectoryEntry::new(
                name,
                EntryNode::File {
                    object: bytes.clone(),
                    mode: 0o644,
                },
            )
            .unwrap(),
        );
    }
    let view = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries,
        })
        .unwrap();
    let source = source_from(store, &view);
    let profile = store
        .put_blob(
            &canonical_json(&rudder_engine::workspace::WorkspaceProfile::source_only()).unwrap(),
        )
        .unwrap();
    let topology = |groups| {
        store
            .put_blob(
                &canonical_json(&HardlinkTopology {
                    format_version: 1,
                    groups,
                })
                .unwrap(),
            )
            .unwrap()
    };
    let unlinked = topology(vec![]);
    let linked = topology(vec![HardlinkGroup {
        paths_hex: vec![vec![hex::encode(b"a")], vec![hex::encode(b"b")]],
    }]);
    let make_checkpoint = |hardlinks| {
        store
            .put_checkpoint_v3(&CheckpointV3Manifest {
                format_version: 3,
                source: source.clone(),
                environment: Some(prepared.root.clone()),
                parents: vec![],
                consistency: CheckpointConsistency::Quiesced,
                durability: Durability::Local,
                knowledge_cursor: 0,
                view_root: view.clone(),
                profile: profile.clone(),
                hardlinks,
            })
            .unwrap()
    };
    let independent = make_checkpoint(unlinked.clone());
    let shared = make_checkpoint(linked.clone());
    let a = rudder_engine::workspace::build_input_view(store, &independent).unwrap();
    let b = rudder_engine::workspace::build_input_view(store, &shared).unwrap();
    assert_eq!(
        a.view_root(),
        b.view_root(),
        "content trees are deliberately identical"
    );
    assert_eq!(a.source_root(), b.source_root());
    assert_eq!(a.hardlinks(), Some(&unlinked));
    assert_eq!(b.hardlinks(), Some(&linked));
    assert_eq!(b.hardlink_group_count(), 1);
    #[cfg(target_os = "linux")]
    {
        let build_profile =
            BuildProfile::from_toml(include_str!("../../examples/build-profile/rudder.toml"))
                .unwrap();
        let directory = _dir.path().join("must-not-materialize-linked-input");
        let frontend = _dir.path().join("not-a-qualified-frontend");
        std::fs::write(&frontend, b"unqualified frontend fixture").unwrap();
        let error = Buck2Adapter::new(&frontend)
            .unwrap()
            .build(&index, &build_profile, &b, &prepared, &directory)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("hardlink-preserving VFS input adapter"));
        assert!(
            !directory.exists(),
            "unsupported input semantics cannot start materialization or frontend work"
        );
    }
    let mut contract = ActionContract {
        format_version: 1,
        frontend: "buck2-rudder-linux-compile-v1".into(),
        frontend_executable: store.put_blob(b"buck2").unwrap(),
        rules: store.put_blob(b"trusted rules").unwrap(),
        source_root: source,
        input_topology: a.hardlinks().cloned(),
        environment_root: prepared.root,
        platform: spec.platform,
        toolchains: spec.toolchains,
        command: vec!["buck2".into(), "build".into()],
        environment: Default::default(),
        outputs: ["result".into()].into_iter().collect(),
        policy: BuildPolicy {
            version: "v1".into(),
            environment: Default::default(),
            effects: Default::default(),
        },
    };
    let key_a = contract.key(store).unwrap();
    contract.input_topology = b.hardlinks().cloned();
    let key_b = contract.key(store).unwrap();
    assert_ne!(key_a, key_b, "file identity must enter action identity");
    store.pin("v3-test-action", &key_b).unwrap();
    store.pin("v3-test-checkpoint", &shared).unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert!(
        store.read_object(&linked).is_ok(),
        "topology survives through both action and checkpoint closures"
    );
    let forged = topology(vec![HardlinkGroup {
        paths_hex: vec![vec![hex::encode(b"a")], vec![hex::encode(b"missing")]],
    }]);
    let invalid = store
        .put_checkpoint_v3(&CheckpointV3Manifest {
            format_version: 3,
            source: contract.source_root,
            environment: Some(contract.environment_root),
            parents: vec![],
            consistency: CheckpointConsistency::Quiesced,
            durability: Durability::Local,
            knowledge_cursor: 0,
            view_root: view,
            profile,
            hardlinks: forged,
        })
        .unwrap();
    assert!(
        rudder_engine::workspace::build_input_view(store, &invalid).is_err(),
        "opaque blob retention cannot assert a valid topology"
    );
}
