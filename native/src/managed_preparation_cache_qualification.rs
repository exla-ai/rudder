use super::*;

fn populated_root(f: &Fixture, entries: usize) -> ObjectRef {
    let path = f.temporary.0.join("qualification-source");
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    for i in 0..entries {
        fs::write(path.join(format!("file-{i:05}")), b"shared test data\n").unwrap();
    }
    let directory = f.store.import_tree(&path, CheckpointConsistency::Quiesced).unwrap();
    let profile = WorkspaceProfile::source_only();
    let policy = SourceProjectionPolicy::from_import_policy(&profile.source_exclusions()).unwrap();
    let policy = f.store.put_blob(&canonical_json(&policy).unwrap()).unwrap();
    let source = f.store.put_metadata(ObjectKind::SourceRoot, &SourceV2 {
        format_version: 2, consistency: CheckpointConsistency::Quiesced,
        directory: directory.clone(), projection_policy: policy,
    }).unwrap();
    let profile = f.store.put_blob(&canonical_json(&profile).unwrap()).unwrap();
    let hardlinks = f.store.put_blob(&canonical_json(&HardlinkTopology { format_version: 1, groups: vec![] }).unwrap()).unwrap();
    let environment = f.store.put_metadata(ObjectKind::EnvironmentRoot, &EnvironmentV2 {
        format_version: 2, directory: directory.clone(),
        platform: BTreeMap::from([("qualification_scope".into(), "metadata-fixture-only".into())]),
        installer_inputs: vec![], provenance: BTreeMap::new(),
    }).unwrap();
    let root = f.store.put_metadata(ObjectKind::RunnableRoot, &RunnableRoot {
        format_version: 1, consistency: CheckpointConsistency::Quiesced,
        source, environment: Some(environment), view_root: directory,
        profile, hardlinks, knowledge_cursor: 0,
    }).unwrap();
    f.store.pin(&format!("qualification:{}", root.digest), &root).unwrap();
    root
}

#[test]
#[ignore = "explicit local preparation benchmark, not a real-agent latency gate"]
fn measured_workspace_preparation_scaling() {
    let mut records = Vec::new();
    for entries in [100, 1_000, 10_000] {
        let mut f = Fixture::new();
        let root = populated_root(&f, entries);
        let runnable = decode_runnable(&f.store.read_object(&root).unwrap()).unwrap();
        let snapshot = VfsCheckpoint { format_version: 1,
            directory: runnable.view_root.clone(), hardlinks: runnable.hardlinks.clone() };
        let mut fresh_ns = Vec::new();
        for i in 0..3 {
            let fence = f.reserve(&root, &format!("fresh-{i}"));
            let start = Instant::now();
            let manager = VfsManager::open(f.temporary.0.join(format!("fresh-manager-{i}")), f.store.clone()).unwrap();
            manager.prepare_paged_snapshot(&snapshot).unwrap();
            let view = manager.fork_paged_snapshot(&uuid::Uuid::new_v4().to_string(), &snapshot, fence.owner_epoch()).unwrap();
            fresh_ns.push(start.elapsed().as_nanos());
            let file = view.lookup(1, b"file-00000").unwrap();
            assert_eq!(view.read(file.ino, 0, 32).unwrap(), b"shared test data\n");
        }
        let mut owners = Vec::new();
        let mut samples = Vec::new();
        for i in 0..11 {
            let fence = f.reserve(&root, &format!("cache-{i}"));
            let budget = FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
            let start = Instant::now();
            let view = f.cache.fork_selected(&fence, &root, &runnable.source,
                &uuid::Uuid::new_v4().to_string(), &f.journal, &AtomicBool::new(false), &budget).unwrap();
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(view.stats().cache_hit, i != 0);
            assert_eq!(view.stats().preparation_calls, 1);
            let file = view.view().lookup(1, b"file-00000").unwrap();
            assert_eq!(view.view().read(file.ino, 0, 32).unwrap(), b"shared test data\n");
            samples.push(serde_json::json!({"ns":elapsed,"hit":view.stats().cache_hit,
                "prepare_ns":view.stats().prepare_ns,"fork_ns":view.stats().fork_ns}));
            owners.push(view);
        }
        assert_eq!(f.cache.counts(), (0, 1));
        records.push(serde_json::json!({"entries":entries,"runnable":root,
            "fresh_manager_prepare_fork_ns":fresh_ns,"validated_cache_fork":samples,
            "simultaneous_live_views":owners.len(),"actual_preparation_calls":1}));
    }
    println!("RUDDER_PREPARATION_BENCH_V1={}", serde_json::json!({
        "format_version":1,"platform":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "scope":"local metadata prepare/fork only; excludes import, mount, backend, VCS seed, remote transfer and builds",
        "timing_boundary":"attempt reservation excluded; fresh includes manager open, cache includes selected-root/owner validation",
        "cases":records}));
}
