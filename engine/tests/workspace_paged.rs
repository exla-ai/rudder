use rudder_engine::{
    content::{ContentStore, FileNode, StagedContentStore, CHUNK_BYTES, FILE_LAYOUT},
    directory::{DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory},
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef},
    snapshot::{self, EnvironmentV2, RunnableRoot, SourceV2},
    store::{DirectoryManifest, LocalStore},
    workspace::{
        runnable_build_input_view, stage_source_complement, ProjectionBudget, WorkspaceProfile,
    },
};
use std::{collections::BTreeMap, time::Duration};

fn setup() -> (tempfile::TempDir, LocalStore) {
    let base = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if std::path::Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    let temp = tempfile::Builder::new()
        .prefix("rudder-workspace-paged-")
        .tempdir_in(base)
        .unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    (temp, store)
}
fn paged(store: &LocalStore, stage: &str, mode: u32, entries: Vec<DirectoryEntry>) -> ObjectRef {
    PagedDirectory::new(&LocalDirectoryStore::new(store, stage))
        .build(mode, entries)
        .unwrap()
}
fn blob(store: &LocalStore, stage: &str, bytes: &[u8]) -> ObjectRef {
    let reference = ObjectRef::from_bytes(ObjectKind::Blob, bytes);
    store.receive_staged(stage, &reference, bytes).unwrap();
    reference
}
fn file(name: &[u8], object: &ObjectRef, size: u64, mode: u32) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryNode::File {
            object: object.clone(),
            logical_size: size,
            mode,
        },
    )
    .unwrap()
}
fn directory(name: &[u8], object: &ObjectRef) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryNode::Directory {
            object: object.clone(),
        },
    )
    .unwrap()
}
fn publish(
    store: &LocalStore,
    stage: &str,
    view: &ObjectRef,
    source: &ObjectRef,
    environment: Option<&ObjectRef>,
    profile: &WorkspaceProfile,
    policy: &ObjectRef,
    hardlinks: Option<&ObjectRef>,
    pin: &str,
) -> ObjectRef {
    let profile = blob(store, stage, &canonical_json(profile).unwrap());
    let hardlinks = hardlinks
        .cloned()
        .unwrap_or_else(|| blob(store, stage, b"{\"format_version\":1,\"groups\":[]}"));
    let source = snapshot::stage(
        store,
        stage,
        ObjectKind::SourceRoot,
        &SourceV2 {
            format_version: 2,
            consistency: CheckpointConsistency::Quiesced,
            directory: source.clone(),
            projection_policy: policy.clone(),
        },
    )
    .unwrap();
    let reference = snapshot::stage(
        store,
        stage,
        ObjectKind::RunnableRoot,
        &RunnableRoot {
            format_version: 1,
            consistency: CheckpointConsistency::Quiesced,
            source,
            environment: environment.cloned(),
            view_root: view.clone(),
            profile,
            hardlinks,
            knowledge_cursor: u64::MAX,
        },
    )
    .unwrap();
    store.publish(stage, pin, &reference).unwrap();
    reference
}
fn environment(store: &LocalStore, stage: &str, root: &ObjectRef) -> ObjectRef {
    snapshot::stage(
        store,
        stage,
        ObjectKind::EnvironmentRoot,
        &EnvironmentV2 {
            format_version: 2,
            directory: root.clone(),
            platform: BTreeMap::from([
                ("os".into(), "linux".into()),
                ("abi".into(), "fixture-only".into()),
            ]),
            installer_inputs: vec![],
            provenance: BTreeMap::new(),
        },
    )
    .unwrap()
}

#[test]
fn qualified_paged_input_uses_actual_file_lengths_and_retains_its_own_root_pin() {
    let (_temp, store) = setup();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let content = StagedContentStore::new(&store, &stage.id).unwrap();
    let chunk = content.put_blob(&vec![b'x'; CHUNK_BYTES]).unwrap();
    let large = content
        .put_file_node(&FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level: 0,
            logical_size: 52 * CHUNK_BYTES as u64,
            children: vec![chunk; 52],
        })
        .unwrap();
    let source = paged(
        &store,
        &stage.id,
        0o751,
        vec![file(b"source-\xff", &large, 52 * CHUNK_BYTES as u64, 0o640)],
    );
    let tools = paged(
        &store,
        &stage.id,
        0o755,
        vec![file(b"compiler", &large, 52 * CHUNK_BYTES as u64, 0o755)],
    );
    let env_dir = paged(&store, &stage.id, 0o755, vec![directory(b"tools", &tools)]);
    let env = environment(&store, &stage.id, &env_dir);
    let scratch = blob(&store, &stage.id, b"dirty scratch");
    let vcs = blob(&store, &stage.id, b"view-only VCS recovery");
    let view = paged(
        &store,
        &stage.id,
        0o751,
        vec![
            file(b"source-\xff", &large, 52 * CHUNK_BYTES as u64, 0o640),
            directory(b"tools", &tools),
            file(b"scratch", &scratch, scratch.size_bytes, 0o600),
            file(b".jj", &vcs, vcs.size_bytes, 0o600),
        ],
    );
    let profile = WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"tools".to_vec()]],
        scratch: vec![vec![b"scratch".to_vec()]],
        vcs_local: vec![vec![b".jj".to_vec()]],
    };
    let policy = blob(
        &store,
        &stage.id,
        b"{\"contract\":\"source-complement-fixture\"}",
    );
    let producer = stage_source_complement(
        &store,
        &stage.id,
        &view,
        &profile,
        &ProjectionBudget::default(),
    )
    .unwrap();
    assert_eq!(producer.directory, source);
    assert!(store.retention().unwrap().staging.contains_key(&stage.id));
    let runnable = publish(
        &store,
        &stage.id,
        &view,
        &source,
        Some(&env),
        &profile,
        &policy,
        None,
        "caller-root",
    );
    let input = runnable_build_input_view(&store, &runnable, &policy, &ProjectionBudget::default())
        .unwrap();
    assert_eq!(input.input().checkpoint(), &runnable);
    assert_eq!(input.input().view_root(), &view);
    assert_eq!(input.input().base_environment(), Some(&env));
    assert!(!input.input().dirty_prepared());
    assert_eq!(input.input().hardlink_group_count(), 0);
    assert!(
        input.report().stats.metadata_read_bytes < 1024 * 1024,
        "metadata proof must not read the4MiB Blob or208MiB logical payload"
    );
    let own_pin = input.retention_pin_id().to_owned();
    assert_ne!(own_pin, "caller-root");
    assert_eq!(store.retention().unwrap().pins[&own_pin], runnable);
    store.unpin("caller-root").unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert!(store.read_object(&large).is_ok());
    drop(input);
    assert!(!store.retention().unwrap().pins.contains_key(&own_pin));
    assert!(store.gc(Duration::ZERO).unwrap().removed_objects > 0);
    assert!(store.read_object(&large).is_err());
}

#[test]
fn prepared_changes_missing_nodes_modes_and_symlink_targets_are_bound_without_following_links() {
    let (_temp, store) = setup();
    let profile = WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"tools".to_vec()], vec![b"missing".to_vec()]],
        scratch: vec![],
        vcs_local: vec![],
    };
    for (case, variant) in [
        "same",
        "bytes",
        "mode",
        "deleted",
        "added-missing",
        "symlink",
    ]
    .into_iter()
    .enumerate()
    {
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let policy = blob(&store, &stage.id, b"{}");
        let bytes = blob(&store, &stage.id, b"compiler");
        let changed = blob(&store, &stage.id, b"different");
        let original = paged(
            &store,
            &stage.id,
            0o755,
            vec![file(b"cc", &bytes, bytes.size_bytes, 0o751)],
        );
        let env_root = paged(
            &store,
            &stage.id,
            0o755,
            vec![directory(b"tools", &original)],
        );
        let env = environment(&store, &stage.id, &env_root);
        let mut entries = vec![];
        match variant {
            "same" => entries.push(directory(b"tools", &original)),
            "bytes" => entries.push(directory(
                b"tools",
                &paged(
                    &store,
                    &stage.id,
                    0o755,
                    vec![file(b"cc", &changed, changed.size_bytes, 0o751)],
                ),
            )),
            "mode" => entries.push(directory(
                b"tools",
                &paged(
                    &store,
                    &stage.id,
                    0o700,
                    vec![file(b"cc", &bytes, bytes.size_bytes, 0o751)],
                ),
            )),
            "added-missing" => {
                entries.push(directory(b"tools", &original));
                entries.push(file(b"missing", &bytes, bytes.size_bytes, 0o600));
            }
            "symlink" => entries.push(
                DirectoryEntry::new(
                    b"tools",
                    EntryNode::Symlink {
                        target_hex: hex::encode(b"/outside/unqualified"),
                    },
                )
                .unwrap(),
            ),
            _ => {}
        }
        let view = paged(&store, &stage.id, 0o755, entries);
        let source = paged(&store, &stage.id, 0o755, vec![]);
        let runnable = publish(
            &store,
            &stage.id,
            &view,
            &source,
            Some(&env),
            &profile,
            &policy,
            None,
            &format!("case-{case}"),
        );
        let proof =
            runnable_build_input_view(&store, &runnable, &policy, &ProjectionBudget::default())
                .unwrap();
        assert_eq!(
            proof.input().dirty_prepared(),
            variant != "same",
            "{variant}"
        );
    }
}

#[test]
fn forged_source_bad_policy_bad_topology_and_budget_errors_never_leak_owned_pins() {
    let (_temp, store) = setup();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let bytes = blob(&store, &stage.id, b"source");
    let view = paged(
        &store,
        &stage.id,
        0o755,
        vec![file(b"source", &bytes, bytes.size_bytes, 0o644)],
    );
    let empty = paged(&store, &stage.id, 0o755, vec![]);
    let policy = blob(&store, &stage.id, b"{}");
    let runnable = publish(
        &store,
        &stage.id,
        &view,
        &empty,
        None,
        &WorkspaceProfile::source_only(),
        &policy,
        None,
        "forged-source",
    );
    let original = store.retention().unwrap().pins;
    assert!(
        runnable_build_input_view(&store, &runnable, &policy, &ProjectionBudget::default())
            .is_err()
    );
    assert_eq!(store.retention().unwrap().pins, original);
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let invalid_policy = blob(&store, &stage.id, b"{ }");
    let malformed = publish(
        &store,
        &stage.id,
        &view,
        &view,
        None,
        &WorkspaceProfile::source_only(),
        &invalid_policy,
        None,
        "noncanonical-policy",
    );
    let original = store.retention().unwrap().pins;
    assert!(runnable_build_input_view(
        &store,
        &malformed,
        &invalid_policy,
        &ProjectionBudget::default()
    )
    .is_err());
    assert_eq!(store.retention().unwrap().pins, original);
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let valid = publish(
        &store,
        &stage.id,
        &view,
        &view,
        None,
        &WorkspaceProfile::source_only(),
        &policy,
        None,
        "valid",
    );
    assert!(runnable_build_input_view(
        &store,
        &valid,
        &invalid_policy,
        &ProjectionBudget::default()
    )
    .is_err());
    let before = store.retention().unwrap().pins;
    let budget = ProjectionBudget {
        max_metadata_bytes: 0,
        ..ProjectionBudget::default()
    };
    assert!(runnable_build_input_view(&store, &valid, &policy, &budget).is_err());
    assert_eq!(store.retention().unwrap().pins, before);
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let bad_topology=blob(&store,&stage.id,b"{\"format_version\":1,\"groups\":[{\"paths_hex\":[[\"736f75726365\"],[\"6d697373696e67\"]]}]}");
    let forged = publish(
        &store,
        &stage.id,
        &view,
        &view,
        None,
        &WorkspaceProfile::source_only(),
        &policy,
        Some(&bad_topology),
        "bad-topology",
    );
    let before = store.retention().unwrap().pins;
    assert!(
        runnable_build_input_view(&store, &forged, &policy, &ProjectionBudget::default()).is_err()
    );
    assert_eq!(store.retention().unwrap().pins, before);
}

#[test]
fn source_projection_is_path_bounded_for_large_paged_directory_and_never_starts_or_ends_staging() {
    let (_temp, store) = setup();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let bytes = blob(&store, &stage.id, b"one");
    let source_entries = (0..25_000)
        .map(|i| {
            file(
                format!("file-{i:05}").as_bytes(),
                &bytes,
                bytes.size_bytes,
                0o644,
            )
        })
        .collect::<Vec<_>>();
    let source = paged(&store, &stage.id, 0o755, source_entries.clone());
    let mut entries = source_entries;
    entries.push(file(b"prepared", &bytes, bytes.size_bytes, 0o600));
    entries.push(file(b"scratch", &bytes, bytes.size_bytes, 0o600));
    let view = paged(&store, &stage.id, 0o755, entries);
    let profile = WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"prepared".to_vec()]],
        scratch: vec![vec![b"scratch".to_vec()]],
        vcs_local: vec![],
    };
    let pins = store.retention().unwrap().pins;
    let result = stage_source_complement(
        &store,
        &stage.id,
        &view,
        &profile,
        &ProjectionBudget::default(),
    )
    .unwrap();
    assert_eq!(result.directory, source);
    assert_eq!(result.report.stats.legacy_entries_scanned, 0);
    assert!(result.report.stats.metadata_reads < 50);
    assert!(result.report.stats.encoded_metadata_objects < 20);
    assert!(result.report.stats.metadata_read_bytes < 1024 * 1024);
    assert_eq!(store.retention().unwrap().pins, pins);
    assert!(store.retention().unwrap().staging.contains_key(&stage.id));
    eprintln!(
        "paged-source-projection:{}",
        serde_json::to_string(&result.report).unwrap()
    );
    let policy = blob(&store, &stage.id, b"{}");
    let runnable = publish(
        &store, &stage.id, &view, &source, None, &profile, &policy, None, "large",
    );
    let proof = runnable_build_input_view(&store, &runnable, &policy, &ProjectionBudget::default())
        .unwrap();
    assert!(proof.input().dirty_prepared());
    assert!(proof.report().stats.metadata_reads < 60);
    assert_eq!(proof.report().stats.legacy_entries_scanned, 0);
    eprintln!(
        "paged-runnable-proof:{}",
        serde_json::to_string(proof.report()).unwrap()
    );
}

#[test]
fn mixed_legacy_and_paged_directories_keep_legacy_identity_and_traversal_stops_at_symlinks() {
    use rudder_engine::store::{DirectoryEntry as LegacyEntry, EntryNode as LegacyNode};
    let (_temp, store) = setup();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let bytes = blob(&store, &stage.id, b"small");
    let deps = paged(
        &store,
        &stage.id,
        0o711,
        vec![
            file(b"node_modules", &bytes, bytes.size_bytes, 0o600),
            file(b"source", &bytes, bytes.size_bytes, 0o644),
        ],
    );
    let kept = paged(
        &store,
        &stage.id,
        0o711,
        vec![file(b"source", &bytes, bytes.size_bytes, 0o644)],
    );
    let legacy = |deps: ObjectRef| {
        store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries: vec![
                    LegacyEntry::new(b"deps", LegacyNode::Directory { object: deps }).unwrap(),
                ],
            })
            .unwrap()
    };
    let view = legacy(deps);
    let source = legacy(kept);
    let profile = WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"deps".to_vec(), b"node_modules".to_vec()]],
        scratch: vec![],
        vcs_local: vec![],
    };
    let result = stage_source_complement(
        &store,
        &stage.id,
        &view,
        &profile,
        &ProjectionBudget::default(),
    )
    .unwrap();
    assert_eq!(result.directory, source);
    assert!(result.report.stats.legacy_entries_scanned > 0);
    let policy = blob(&store, &stage.id, b"{}");
    let runnable = publish(
        &store, &stage.id, &view, &source, None, &profile, &policy, None, "mixed",
    );
    assert!(
        runnable_build_input_view(&store, &runnable, &policy, &ProjectionBudget::default())
            .unwrap()
            .input()
            .dirty_prepared()
    );
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let linked = paged(
        &store,
        &stage.id,
        0o755,
        vec![DirectoryEntry::new(
            b"deps",
            EntryNode::Symlink {
                target_hex: hex::encode(b"outside"),
            },
        )
        .unwrap()],
    );
    assert!(stage_source_complement(
        &store,
        &stage.id,
        &linked,
        &profile,
        &ProjectionBudget::default()
    )
    .is_err());
    store.abandon_staging(&stage.id).unwrap();
}

#[test]
fn unretained_runnable_roots_never_enter_new_authority() {
    let (_temp, store) = setup();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let root = paged(&store, &stage.id, 0o755, vec![]);
    let policy = blob(&store, &stage.id, b"{}");
    let profile = blob(
        &store,
        &stage.id,
        &canonical_json(&WorkspaceProfile::source_only()).unwrap(),
    );
    let topology = blob(&store, &stage.id, b"{\"format_version\":1,\"groups\":[]}");
    let source = snapshot::stage(
        &store,
        &stage.id,
        ObjectKind::SourceRoot,
        &SourceV2 {
            format_version: 2,
            consistency: CheckpointConsistency::Quiesced,
            directory: root.clone(),
            projection_policy: policy.clone(),
        },
    )
    .unwrap();
    let reference = snapshot::stage(
        &store,
        &stage.id,
        ObjectKind::RunnableRoot,
        &RunnableRoot {
            format_version: 1,
            consistency: CheckpointConsistency::Quiesced,
            source,
            environment: None,
            view_root: root,
            profile,
            hardlinks: topology,
            knowledge_cursor: 170,
        },
    )
    .unwrap();
    assert!(
        runnable_build_input_view(&store, &reference, &policy, &ProjectionBudget::default())
            .is_err()
    );
    assert!(store.retention().unwrap().pins.is_empty());
    store.abandon_staging(&stage.id).unwrap();
}

#[test]
fn hardlink_topology_is_validated_and_retained_separately_from_prepared_data_fingerprint() {
    let (_temp, store) = setup();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let bytes = blob(&store, &stage.id, b"same bytes, declared real link");
    let root = paged(
        &store,
        &stage.id,
        0o755,
        vec![
            file(b"a", &bytes, bytes.size_bytes, 0o640),
            file(b"alias", &bytes, bytes.size_bytes, 0o640),
        ],
    );
    let policy = blob(&store, &stage.id, b"{}");
    let topology = blob(
        &store,
        &stage.id,
        b"{\"format_version\":1,\"groups\":[{\"paths_hex\":[[\"61\"],[\"616c696173\"]]}]}",
    );
    let linked = publish(
        &store,
        &stage.id,
        &root,
        &root,
        None,
        &WorkspaceProfile::source_only(),
        &policy,
        Some(&topology),
        "linked",
    );
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let separate = publish(
        &store,
        &stage.id,
        &root,
        &root,
        None,
        &WorkspaceProfile::source_only(),
        &policy,
        None,
        "separate",
    );
    let linked_proof =
        runnable_build_input_view(&store, &linked, &policy, &ProjectionBudget::default()).unwrap();
    let independent_proof =
        runnable_build_input_view(&store, &separate, &policy, &ProjectionBudget::default())
            .unwrap();
    assert_eq!(
        linked_proof.input().view_root(),
        independent_proof.input().view_root()
    );
    assert_eq!(
        linked_proof.input().effective_prepared_fingerprint(),
        independent_proof.input().effective_prepared_fingerprint()
    );
    assert_eq!(linked_proof.input().hardlinks(), Some(&topology));
    assert_eq!(linked_proof.input().hardlink_group_count(), 1);
    assert_eq!(linked_proof.report().stats.topology_paths, 2);
    assert_eq!(independent_proof.input().hardlink_group_count(), 0);
    store.unpin("linked").unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert!(store.read_object(&topology).is_ok());
    drop(linked_proof);
    store.gc(Duration::ZERO).unwrap();
    assert!(store.read_object(&topology).is_err());
    assert!(store.read_object(&root).is_ok());
    let tiny = ProjectionBudget {
        max_topology_paths: 1,
        ..ProjectionBudget::default()
    };
    assert!(runnable_build_input_view(&store, &separate, &policy, &tiny).is_ok());
}

#[test]
fn raw_partition_boundaries_and_metadata_budgets_are_exact_and_leave_caller_staging_intact() {
    let (_temp, store) = setup();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let bytes = blob(&store, &stage.id, b"source");
    let child = paged(
        &store,
        &stage.id,
        0o755,
        vec![file(b"private", &bytes, bytes.size_bytes, 0o600)],
    );
    let view = paged(
        &store,
        &stage.id,
        0o755,
        vec![
            directory(b"foo", &child),
            file(b"foo-bar", &bytes, bytes.size_bytes, 0o644),
            file(b"raw-\xff", &bytes, bytes.size_bytes, 0o644),
        ],
    );
    let expected = paged(
        &store,
        &stage.id,
        0o755,
        vec![
            file(b"foo-bar", &bytes, bytes.size_bytes, 0o644),
            file(b"raw-\xff", &bytes, bytes.size_bytes, 0o644),
        ],
    );
    let profile = WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"foo".to_vec()]],
        scratch: vec![],
        vcs_local: vec![],
    };
    let before = store.retention().unwrap().pins;
    for budget in [
        ProjectionBudget {
            max_metadata_bytes: 0,
            ..ProjectionBudget::default()
        },
        ProjectionBudget {
            max_encoded_bytes: 0,
            ..ProjectionBudget::default()
        },
        ProjectionBudget {
            max_namespace_steps: 0,
            ..ProjectionBudget::default()
        },
        ProjectionBudget {
            max_partition_paths: 0,
            ..ProjectionBudget::default()
        },
    ] {
        assert!(stage_source_complement(&store, &stage.id, &view, &profile, &budget).is_err());
        assert!(store.retention().unwrap().staging.contains_key(&stage.id));
        assert_eq!(store.retention().unwrap().pins, before);
    }
    let projected = stage_source_complement(
        &store,
        &stage.id,
        &view,
        &profile,
        &ProjectionBudget::default(),
    )
    .unwrap();
    assert_eq!(projected.directory, expected);
    let overlapping = WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![b"foo".to_vec()]],
        scratch: vec![vec![b"foo".to_vec(), b"child".to_vec()]],
        vcs_local: vec![],
    };
    assert!(stage_source_complement(
        &store,
        &stage.id,
        &view,
        &overlapping,
        &ProjectionBudget::default()
    )
    .is_err());
    let invalid = WorkspaceProfile {
        format_version: 1,
        prepared: vec![vec![vec![b'x'; 256]]],
        scratch: vec![],
        vcs_local: vec![],
    };
    assert!(stage_source_complement(
        &store,
        &stage.id,
        &view,
        &invalid,
        &ProjectionBudget::default()
    )
    .is_err());
    store.abandon_staging(&stage.id).unwrap();
    assert!(stage_source_complement(
        &store,
        &stage.id,
        &view,
        &profile,
        &ProjectionBudget::default()
    )
    .is_err());
}
