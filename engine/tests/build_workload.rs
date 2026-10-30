//! Opt-in real Linux compiler/REAPI experiment. Large fixtures must be placed
//! on the caller's owned workspace volume, never the worker's small /tmp.
use rudder_engine::build::*;
use rudder_engine::protocol::{CheckpointConsistency, ObjectRef, FORMAT_VERSION};
use rudder_engine::store::{
    CheckpointV2Manifest, DirectoryEntry, EntryNode, EnvironmentManifest, LocalStore,
    SourceManifest,
};
use rudder_engine::workspace::WorkspaceProfile;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

struct CacheProcess(Child);
impl Drop for CacheProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Daemons {
    binary: PathBuf,
    workspaces: Vec<PathBuf>,
}
impl Drop for Daemons {
    fn drop(&mut self) {
        for path in &self.workspaces {
            let _ = Command::new(&self.binary)
                .arg("kill")
                .current_dir(path)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", path.join(".driver/home"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn snapshot(store: &LocalStore, base: &Path, rust: &str, lock: &str) -> ObjectRef {
    let path = base.join(format!("source-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(path.join("rust")).unwrap();
    fs::create_dir(path.join("typescript")).unwrap();
    fs::write(path.join("rust/lib.rs"), rust).unwrap();
    fs::write(
        path.join("typescript/main.ts"),
        include_str!("../../examples/build-profile/typescript/main.ts"),
    )
    .unwrap();
    fs::write(path.join("Cargo.lock"), lock).unwrap();
    fs::write(
        path.join("package-lock.json"),
        include_str!("../../examples/build-profile/package-lock.json"),
    )
    .unwrap();
    let directory = store
        .import_tree(&path, CheckpointConsistency::Quiesced)
        .unwrap();
    store
        .put_source(&SourceManifest {
            format_version: FORMAT_VERSION,
            directory,
            parents: vec![],
            consistency: CheckpointConsistency::Quiesced,
        })
        .unwrap()
}
fn specification(
    store: &LocalStore,
    profile: &BuildProfile,
    directory: &ObjectRef,
    prepared_path: &Path,
    tools: &Value,
) -> PreparationSpec {
    let paths = tools["compilers"]["compiler_paths"].as_object().unwrap();
    let toolchains = paths
        .iter()
        .map(|(name, path)| {
            let relative = path.as_str().unwrap();
            let mut parent = directory.clone();
            let parts: Vec<_> = relative.split('/').collect();
            let mut executable = None;
            for (index, part) in parts.iter().enumerate() {
                let entry = rudder_engine::tree::lookup(store, &parent, part.as_bytes())
                    .unwrap()
                    .unwrap();
                match entry.node {
                    rudder_engine::directory::EntryNode::Directory { object }
                        if index + 1 < parts.len() =>
                    {
                        parent = object
                    }
                    rudder_engine::directory::EntryNode::File { object, .. }
                        if index + 1 == parts.len() =>
                    {
                        executable = Some(object)
                    }
                    _ => panic!("prepared compiler path is not an ordinary file"),
                }
            }
            assert!(prepared_path.join(relative).is_file());
            (
                name.clone(),
                ToolchainIdentity {
                    version: tools["compilers"]
                        .get(if name == "rust" { "rustc" } else { name })
                        .and_then(Value::as_str)
                        .unwrap_or("prepared-runtime-v2")
                        .into(),
                    closure: directory.clone(),
                    executable: executable.unwrap(),
                    executable_path: relative.into(),
                    configuration: BTreeMap::new(),
                },
            )
        })
        .collect();
    let installer_inputs = [
        (
            "Cargo.lock",
            include_bytes!("../../examples/build-profile/Cargo.lock").as_slice(),
        ),
        (
            "package-lock.json",
            include_bytes!("../../examples/build-profile/package-lock.json").as_slice(),
        ),
        (
            "tools.lock.json",
            include_bytes!("../../examples/build-profile/tools.lock.json").as_slice(),
        ),
        (
            "prepare_tools.py",
            include_bytes!("../../examples/build-profile/prepare_tools.py").as_slice(),
        ),
    ]
    .into_iter()
    .map(|(name, bytes)| (name.into(), store.put_blob(bytes).unwrap()))
    .collect();
    PreparationSpec {
        format_version: FORMAT_VERSION,
        platform: profile.platform.clone(),
        base_runtime: directory.clone(),
        installer_inputs,
        installer_commands: vec![vec![
            "python3".into(),
            "prepare_tools.py".into(),
            "--directory".into(),
            "<owned-tool-cache>".into(),
        ]],
        installer_environment: BTreeMap::new(),
        toolchains,
        policy_version: "managed-toolchain-snapshot-v2".into(),
        effects: Default::default(),
    }
}
fn record(
    base: &Path,
    name: &str,
    index: &BuildIndex,
    run: &BuildRun,
    cases: &mut Vec<Value>,
    wall_ms: u64,
) {
    let raw = base.join("evidence").join(name);
    fs::create_dir_all(&raw).unwrap();
    fs::write(
        raw.join("run.json"),
        serde_json::to_vec_pretty(run).unwrap(),
    )
    .unwrap();
    fs::write(
        raw.join("events.json.gz"),
        index
            .store()
            .read_object(&run.observation.event_log)
            .unwrap(),
    )
    .unwrap();
    fs::write(
        raw.join("report.json"),
        index
            .store()
            .read_object(&run.observation.build_report)
            .unwrap(),
    )
    .unwrap();
    cases.push(json!({"case":name,"wall_ms":wall_ms,"stage_ms":run.stage_ms,"frontend_ms":run.frontend_elapsed_ms,"environment_validation_ms":run.environment_materialization_ms,"source_materialization_ms":run.source_materialization_ms,"executed_actions":run.observation.executed_actions,"remote_cache_hits":run.observation.remote_cache_hits,"graph_reused_actions":run.frontend_graph_reused_actions,"source_action_key":run.stored.key,"prepared_output":run.stored.outputs,"run_receipt":run.run_receipt,"native_action_digests":run.observation.native_action_digests,"trace_id":run.observation.trace_id}));
    fs::write(
        base.join("cases-in-progress.json"),
        serde_json::to_vec_pretty(cases).unwrap(),
    )
    .unwrap();
}
fn timed(
    adapter: &Buck2Adapter,
    index: &BuildIndex,
    profile: &BuildProfile,
    source: &ObjectRef,
    prepared: &PreparedEnvironment,
    path: &Path,
) -> (BuildRun, u64) {
    let start = Instant::now();
    let input = checkpoint_view(index.store(), source, prepared);
    let result = adapter
        .build(index, profile, &input, prepared, path)
        .unwrap();
    (result, start.elapsed().as_millis() as u64)
}

fn checkpoint_view(
    store: &LocalStore,
    source: &ObjectRef,
    environment: &PreparedEnvironment,
) -> BuildInputView {
    let source_manifest: SourceManifest =
        serde_json::from_slice(&store.read_object(source).unwrap()).unwrap();
    let environment_manifest: EnvironmentManifest =
        serde_json::from_slice(&store.read_object(&environment.root).unwrap()).unwrap();
    let mut source_directory = store.read_directory(&source_manifest.directory).unwrap();
    let prepared_directory = store
        .read_directory(&environment_manifest.directory)
        .unwrap();
    let mut prepared_paths: Vec<_> = prepared_directory
        .entries
        .iter()
        .map(|entry| vec![entry.name().unwrap()])
        .collect();
    if !prepared_paths.contains(&vec![b"node_modules".to_vec()]) {
        prepared_paths.push(vec![b"node_modules".to_vec()]);
    }
    let profile = WorkspaceProfile {
        format_version: FORMAT_VERSION,
        prepared: prepared_paths,
        scratch: vec![],
        vcs_local: vec![],
    };
    source_directory.entries.extend(prepared_directory.entries);
    let view_root = store.put_directory(source_directory).unwrap();
    let profile_ref = store
        .put_blob(&rudder_engine::protocol::canonical_json(&profile).unwrap())
        .unwrap();
    let checkpoint = store
        .put_checkpoint_v2(&CheckpointV2Manifest {
            format_version: 2,
            source: source.clone(),
            environment: Some(environment.root.clone()),
            parents: vec![],
            consistency: CheckpointConsistency::Quiesced,
            durability: rudder_engine::protocol::Durability::Local,
            knowledge_cursor: 0,
            view_root,
            profile: profile_ref,
        })
        .unwrap();
    rudder_engine::workspace::build_input_view(store, &checkpoint).unwrap()
}

#[test]
#[ignore = "real pinned Rust/TypeScript/Buck2/REAPI workload; run examples/build-profile/run.py on an owned Linux workspace volume"]
fn qualified_rust_typescript_cache_workload() {
    assert!(cfg!(target_os = "linux"));
    let tools_path = PathBuf::from(
        std::env::var_os("RUDDER_BUILD_TOOLS_JSON").expect("prepared toolchain metadata"),
    );
    let tools: Value = serde_json::from_slice(&fs::read(tools_path).unwrap()).unwrap();
    let base = PathBuf::from(
        std::env::var_os("RUDDER_BUILD_WORKLOAD_ROOT").expect("owned workload output directory"),
    );
    assert!(
        base.is_absolute() && !base.starts_with(std::env::temp_dir()),
        "large fixtures must be on an owned workspace volume"
    );
    if base.exists() {
        assert!(
            fs::read_dir(&base).unwrap().next().is_none(),
            "workload output directory must be fresh"
        );
    } else {
        fs::create_dir_all(&base).unwrap();
    }
    let store = LocalStore::open(base.join("cas")).unwrap();
    let staging = store.begin_staging(Duration::from_secs(3600)).unwrap();
    let preparation_start = Instant::now();
    let prepared_path = PathBuf::from(tools["prepared"].as_str().unwrap());
    let directory = store
        .import_tree(&prepared_path, CheckpointConsistency::Quiesced)
        .unwrap();
    let mut profile =
        BuildProfile::from_toml(include_str!("../../examples/build-profile/rudder.toml")).unwrap();
    assert_eq!(
        serde_json::to_value(profile.platform.properties()).unwrap(),
        tools["compilers"]["platform"]
    );
    let spec = specification(&store, &profile, &directory, &prepared_path, &tools);
    let index = BuildIndex::open(base.join("index"), store).unwrap();
    let prepared = index.publish_prepared(&spec, &directory).unwrap();
    let cold_preparation_ms = preparation_start.elapsed().as_millis();
    let started = Instant::now();
    assert_eq!(index.prepared(&spec).unwrap(), Some(prepared.clone()));
    let warm_preparation_lookup_ms = started.elapsed().as_millis();
    let grpc = port();
    let mut http = port();
    while http == grpc {
        http = port();
    }
    let log = File::create(base.join("reapi-cache.log")).unwrap();
    let cache = Command::new(tools["bazel_remote"].as_str().unwrap())
        .args([
            "--dir",
            base.join("reapi-cache").to_str().unwrap(),
            "--max_size",
            "1",
            "--grpc_address",
            &format!("127.0.0.1:{grpc}"),
            "--http_address",
            &format!("127.0.0.1:{http}"),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let _cache = CacheProcess(cache);
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", grpc)).is_err() {
        assert!(Instant::now() < deadline, "REAPI service did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    profile.buck2.remote_cache = Some(RemoteCacheConfig {
        cas_address: format!("grpc://127.0.0.1:{grpc}"),
        action_cache_address: format!("grpc://127.0.0.1:{grpc}"),
        instance_name: "rudder-qualified-profile".into(),
        tls: false,
    });
    let binary = PathBuf::from(tools["buck2"].as_str().unwrap());
    let adapter = Buck2Adapter::new(&binary).unwrap();
    let mut daemons = Daemons {
        binary: binary.clone(),
        workspaces: vec![],
    };
    let mut cases = vec![];
    let rust = include_str!("../../examples/build-profile/rust/lib.rs");
    let lock = include_str!("../../examples/build-profile/Cargo.lock");
    let source = snapshot(index.store(), &base, rust, lock);
    let cold = base.join("cold-view");
    daemons.workspaces.push(cold.clone());
    let (first, wall) = timed(&adapter, &index, &profile, &source, &prepared, &cold);
    assert_eq!(first.observation.executed_actions, 2);
    assert_eq!(first.observation.remote_cache_hits, 0);
    record(&base, "cold-native-cache", &index, &first, &mut cases, wall);
    let (warm, wall) = timed(&adapter, &index, &profile, &source, &prepared, &cold);
    assert_eq!(warm.observation.executed_actions, 0);
    assert_eq!(warm.frontend_graph_reused_actions, 2);
    record(&base, "warm-same-graph", &index, &warm, &mut cases, wall);
    adapter.stop(&cold).unwrap();
    let fresh = base.join("fresh-view");
    daemons.workspaces.push(fresh.clone());
    let (fresh_run, wall) = timed(&adapter, &index, &profile, &source, &prepared, &fresh);
    assert_eq!(fresh_run.observation.executed_actions, 0);
    assert_eq!(fresh_run.observation.remote_cache_hits, 2);
    assert_eq!(fresh_run.stored.outputs, first.stored.outputs);
    assert_eq!(
        fresh_run.observation.native_action_digests,
        first.observation.native_action_digests
    );
    record(
        &base,
        "fresh-graph-reapi-hit",
        &index,
        &fresh_run,
        &mut cases,
        wall,
    );
    adapter.stop(&fresh).unwrap();
    // The same base environment already has a verified cache hit. A dirty
    // prepared overlay must be rejected before that base can be reused.
    let clean_input = checkpoint_view(index.store(), &source, &prepared);
    assert!(!clean_input.dirty_prepared());
    let clean_checkpoint: CheckpointV2Manifest =
        serde_json::from_slice(&index.store().read_object(clean_input.checkpoint()).unwrap())
            .unwrap();
    let empty_modules = index
        .store()
        .put_directory(rudder_engine::store::DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![],
        })
        .unwrap();
    let mut dirty_checkpoint = clean_checkpoint.clone();
    dirty_checkpoint.view_root = index
        .store()
        .update_path(
            &clean_checkpoint.view_root,
            &[b"node_modules".to_vec()],
            Some(
                DirectoryEntry::new(
                    b"node_modules",
                    EntryNode::Directory {
                        object: empty_modules,
                    },
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let dirty_ref = index.store().put_checkpoint_v2(&dirty_checkpoint).unwrap();
    let dirty_input =
        rudder_engine::workspace::build_input_view(index.store(), &dirty_ref).unwrap();
    assert!(dirty_input.dirty_prepared());
    let dirty_destination = base.join("dirty-prepared-denied");
    let dirty_error = adapter
        .build(
            &index,
            &profile,
            &dirty_input,
            &prepared,
            &dirty_destination,
        )
        .unwrap_err()
        .to_string();
    assert!(dirty_error.contains("dirty prepared"));
    assert!(!dirty_destination.exists());
    fs::write(base.join("dirty-prepared-rejection.txt"), &dirty_error).unwrap();
    let edited = snapshot(index.store(), &base, &rust.replace("42", "43"), lock);
    assert_eq!(index.prepared(&spec).unwrap(), Some(prepared.clone()));
    let path = base.join("source-edit");
    daemons.workspaces.push(path.clone());
    let (edited_run, wall) = timed(&adapter, &index, &profile, &edited, &prepared, &path);
    assert_eq!(edited_run.observation.executed_actions, 1);
    assert_eq!(edited_run.observation.remote_cache_hits, 1);
    assert_ne!(edited_run.stored.outputs, first.stored.outputs);
    record(
        &base,
        "source-edit-preparation-hit",
        &index,
        &edited_run,
        &mut cases,
        wall,
    );
    adapter.stop(&path).unwrap();
    let lock2 = format!("{lock}\n# installer lock revision 2\n");
    let locked_source = snapshot(index.store(), &base, &rust.replace("42", "43"), &lock2);
    let mut locked_spec = spec.clone();
    locked_spec.installer_inputs.insert(
        "Cargo.lock".into(),
        index.store().put_blob(lock2.as_bytes()).unwrap(),
    );
    assert!(index.prepared(&locked_spec).unwrap().is_none());
    let locked = index.publish_prepared(&locked_spec, &directory).unwrap();
    let path = base.join("lock-edit");
    daemons.workspaces.push(path.clone());
    let (locked_run, wall) = timed(&adapter, &index, &profile, &locked_source, &locked, &path);
    assert_eq!(locked_run.observation.executed_actions, 2);
    record(
        &base,
        "lockfile-invalidation",
        &index,
        &locked_run,
        &mut cases,
        wall,
    );
    adapter.stop(&path).unwrap();
    // Change real executable bytes while preserving a runnable ELF image. This
    // tests content identity rather than trusting a compiler version label.
    let mut compiler_bytes = index
        .store()
        .read_object(&locked_spec.toolchains["rust"].executable)
        .unwrap();
    compiler_bytes.extend_from_slice(b"\0Rudder compiler byte identity fixture v2\n");
    let compiler = index.store().put_blob(&compiler_bytes).unwrap();
    let compiler_directory = index
        .store()
        .update_path(
            &directory,
            &[
                b"tools".to_vec(),
                b"rust".to_vec(),
                b"bin".to_vec(),
                b"rustc".to_vec(),
            ],
            Some(
                DirectoryEntry::new(
                    b"rustc",
                    EntryNode::File {
                        object: compiler.clone(),
                        mode: 0o755,
                    },
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let mut compiler_spec = locked_spec.clone();
    compiler_spec.base_runtime = compiler_directory.clone();
    for tool in compiler_spec.toolchains.values_mut() {
        tool.closure = compiler_directory.clone();
    }
    compiler_spec.toolchains.get_mut("rust").unwrap().executable = compiler;
    assert!(index.prepared(&compiler_spec).unwrap().is_none());
    let compiler_environment = index
        .publish_prepared(&compiler_spec, &compiler_directory)
        .unwrap();
    let path = base.join("compiler-edit");
    daemons.workspaces.push(path.clone());
    let (compiler_run, wall) = timed(
        &adapter,
        &index,
        &profile,
        &locked_source,
        &compiler_environment,
        &path,
    );
    assert_eq!(compiler_run.observation.executed_actions, 2);
    record(
        &base,
        "compiler-byte-invalidation",
        &index,
        &compiler_run,
        &mut cases,
        wall,
    );
    adapter.stop(&path).unwrap();
    let mut platform_profile = profile.clone();
    platform_profile
        .platform
        .properties
        .insert("abi_contract_revision".into(), "fixture-v2".into());
    let mut platform_spec = locked_spec.clone();
    platform_spec.platform = platform_profile.platform.clone();
    let platform_env = index.publish_prepared(&platform_spec, &directory).unwrap();
    let path = base.join("platform-edit");
    daemons.workspaces.push(path.clone());
    let (platform_run, wall) = timed(
        &adapter,
        &index,
        &platform_profile,
        &locked_source,
        &platform_env,
        &path,
    );
    assert_eq!(platform_run.observation.executed_actions, 2);
    record(
        &base,
        "platform-contract-invalidation",
        &index,
        &platform_run,
        &mut cases,
        wall,
    );
    adapter.stop(&path).unwrap();
    let mut policy_profile = profile.clone();
    policy_profile.policy.version = "linux-managed-compile-v2-fixture".into();
    let path = base.join("policy-edit");
    daemons.workspaces.push(path.clone());
    let (policy_run, wall) = timed(
        &adapter,
        &index,
        &policy_profile,
        &locked_source,
        &locked,
        &path,
    );
    assert_eq!(policy_run.observation.executed_actions, 2);
    record(
        &base,
        "build-policy-invalidation",
        &index,
        &policy_run,
        &mut cases,
        wall,
    );
    adapter.stop(&path).unwrap();
    let concurrent_source = snapshot(index.store(), &base, &rust.replace("42", "99"), lock);
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = vec![];
    for i in 0..2 {
        let path = base.join(format!("concurrent-{i}"));
        daemons.workspaces.push(path.clone());
        let root = base.clone();
        let binary = binary.clone();
        let source = concurrent_source.clone();
        let profile = profile.clone();
        let prepared = prepared.clone();
        let ready = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let index = BuildIndex::open(
                root.join("index"),
                LocalStore::open(root.join("cas")).unwrap(),
            )
            .unwrap();
            let adapter = Buck2Adapter::new(binary).unwrap();
            ready.wait();
            let result = timed(&adapter, &index, &profile, &source, &prepared, &path);
            adapter.stop(path).unwrap();
            result
        }));
    }
    let concurrent: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        concurrent
            .iter()
            .map(|(run, _)| run.observation.executed_actions)
            .sum::<u64>(),
        1,
        "identical concurrent misses must coalesce before frontend execution"
    );
    for (i, (run, wall)) in concurrent.iter().enumerate() {
        record(
            &base,
            &format!("coalesced-miss-{i}"),
            &index,
            run,
            &mut cases,
            *wall,
        );
    }
    let db = rusqlite::Connection::open(base.join("index/index.sqlite")).unwrap();
    let count = || {
        db.query_row(
            "SELECT count(*) FROM build_index WHERE category='action'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    let before = count();
    // Unsupported source symlinks fail on their first build, not only after
    // a source-materialization cache has been populated.
    let base_source: SourceManifest =
        serde_json::from_slice(&index.store().read_object(&source).unwrap()).unwrap();
    let linked_directory = index
        .store()
        .update_path(
            &base_source.directory,
            &[b"unused-link".to_vec()],
            Some(
                DirectoryEntry::new(
                    b"unused-link",
                    EntryNode::Symlink {
                        target_hex: hex::encode(b"/etc/passwd"),
                    },
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let linked_source = index
        .store()
        .put_source(&SourceManifest {
            directory: linked_directory,
            ..base_source
        })
        .unwrap();
    let linked_destination = base.join("source-symlink-denied");
    for _ in 0..2 {
        let error = adapter
            .build(
                &index,
                &profile,
                &checkpoint_view(index.store(), &linked_source, &prepared),
                &prepared,
                &linked_destination,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not admit symlink inputs"));
    }
    assert!(!linked_destination.join(".driver-request.json").exists());
    assert_eq!(count(), before);
    let forbidden = snapshot(
        index.store(),
        &base,
        "pub const SECRET: &str = include_str!(\"/etc/passwd\");\n",
        lock,
    );
    let path = base.join("undeclared-file-read");
    daemons.workspaces.push(path.clone());
    let error = adapter
        .build(
            &index,
            &profile,
            &checkpoint_view(index.store(), &forbidden, &prepared),
            &prepared,
            &path,
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("/etc/passwd") && error.contains("No such file"),
        "undeclared host input must fail inside the compiler namespace: {error}"
    );
    assert_eq!(count(), before);
    fs::write(base.join("undeclared-read-failure.txt"), error).unwrap();
    adapter.stop(&path).unwrap();
    let mut denied = vec![];
    for effect in [
        BuildEffect::Network,
        BuildEffect::Secrets,
        BuildEffect::Signing,
        BuildEffect::Deployment,
        BuildEffect::ExternalState,
        BuildEffect::Clock,
        BuildEffect::Randomness,
    ] {
        let mut candidate = profile.clone();
        candidate.policy.effects.insert(effect);
        let path = base.join(format!("denied-{effect:?}"));
        assert!(adapter
            .build(
                &index,
                &candidate,
                &checkpoint_view(index.store(), &source, &prepared),
                &prepared,
                &path
            )
            .is_err());
        assert!(!path.exists());
        denied.push(effect);
    }
    assert_eq!(count(), before);
    let smoke = base.join("artifact-smoke");
    index
        .store()
        .export_tree(&first.stored.outputs, &smoke)
        .unwrap();
    let rust_bytes = fs::read(smoke.join("rust/rust.rlib")).unwrap();
    assert!(rust_bytes.starts_with(b"!<arch>\n") && rust_bytes.len() > 1024);
    let node = prepared_path.join("tools/node");
    let output = Command::new(node)
        .args([
            "-e",
            "process.stdout.write(String(require(process.argv[1]).answer))",
            smoke.join("typescript/main.js").to_str().unwrap(),
        ])
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"42");
    let summary = json!({"format_version":1,"verified_at_epoch_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64,"platform":tools["compilers"]["platform"],"tools":tools,"cold_preparation_publication_ms":cold_preparation_ms as u64,"warm_preparation_index_lookup_ms":warm_preparation_lookup_ms as u64,"cases":cases,"denied_effects":denied,"artifact_smoke":{"rust_archive_valid":true,"typescript_answer":42},"scope":{"remote_execution":false,"cache_protocol":"native-gRPC-REAPI-v2","backend":"bazel-remote2.6.4","buildbarn_qualified":false,"cross_engine_cache_hits_claimed":false,"shared_divergent_warm_dice_claimed":false,"cold_definition":"empty frontend graph and action cache; OS page cache is not flushed"}});
    fs::write(
        base.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    index.store().abandon_staging(&staging.id).unwrap();
    println!("workload summary: {}", base.join("summary.json").display());
}

/// Independent v2 qualification, with a fresh local index and empty real REAPI
/// service. Its eager materialization cost remains visible in every receipt.
#[test]
#[ignore = "real RunnableRoot/paged/File compiler workload; requires pinned prepared tools and an owned Linux workspace"]
fn runnable_paged_chunked_rust_typescript_cache_workload() {
    use rudder_engine::{
        directory::{LocalDirectoryStore, PagedDirectory},
        protocol::{canonical_json, ObjectKind},
        snapshot::{self, RunnableRoot, SourceV2},
        tree,
        workspace::{self, ProjectionBudget},
    };
    assert!(cfg!(target_os = "linux"));
    let tools: Value = serde_json::from_slice(
        &fs::read(std::env::var_os("RUDDER_BUILD_TOOLS_JSON").expect("pinned prepared tools"))
            .unwrap(),
    )
    .unwrap();
    let base = PathBuf::from(
        std::env::var_os("RUDDER_BUILD_WORKLOAD_ROOT").expect("owned workload output directory"),
    );
    assert!(base.is_absolute() && !base.starts_with(std::env::temp_dir()));
    fs::create_dir_all(&base).unwrap();
    assert!(fs::read_dir(&base).unwrap().next().is_none());
    // Profiling may reuse an explicitly selected, previously qualified local
    // CAS/index. Build workspaces and the real REAPI service are still fresh;
    // the summary records that preparation and materialization were retained.
    let reuse = std::env::var_os("RUDDER_BUILD_REUSE_FIXTURE").map(PathBuf::from);
    let fixture_root = reuse.as_ref().unwrap_or(&base);
    assert!(fixture_root.is_absolute() && !fixture_root.starts_with(std::env::temp_dir()));
    let store = LocalStore::open(fixture_root.join("cas")).unwrap();
    let started = Instant::now();
    let prepared_path = PathBuf::from(tools["prepared"].as_str().unwrap());
    let profile_template =
        BuildProfile::from_toml(include_str!("../../examples/build-profile/rudder.toml")).unwrap();
    let mut profile = profile_template;
    assert_eq!(
        serde_json::to_value(profile.platform.properties()).unwrap(),
        tools["compilers"]["platform"]
    );
    let (index, prepared, spec) = if reuse.is_some() {
        let previous: Value =
            serde_json::from_slice(&fs::read(fixture_root.join("summary.json")).unwrap()).unwrap();
        assert_eq!(
            previous["layout"],
            "runnable-root-v1/source-v2/environment-v2/directory-v2/file-v1"
        );
        let key: ObjectRef =
            serde_json::from_value(previous["cases"][0]["source_action_key"].clone()).unwrap();
        let metadata: rudder_engine::store::ReferencedMetadata =
            serde_json::from_slice(&store.read_object(&key).unwrap()).unwrap();
        let contract: ActionContract =
            serde_json::from_value(metadata.payload["value"].clone()).unwrap();
        let index = BuildIndex::open(fixture_root.join("index"), store).unwrap();
        let prepared = index
            .prepared_root(&contract.environment_root)
            .unwrap()
            .expect("reused preparation must still have retained index authority");
        let environment =
            snapshot::decode_environment(&index.store().read_object(&prepared.root).unwrap())
                .unwrap();
        let spec = specification(
            index.store(),
            &profile,
            &environment.directory,
            &prepared_path,
            &tools,
        );
        (index, prepared, spec)
    } else {
        let staging = store.begin_staging(Duration::from_secs(3600)).unwrap();
        let imported = tree::import_quiesced(
            &store,
            &staging.id,
            &prepared_path,
            &Default::default(),
            &Default::default(),
        )
        .unwrap();
        assert!(
            tree::validate_snapshot(&store, &imported, &Default::default())
                .unwrap()
                .groups
                .is_empty(),
            "environment inode topology needs a separate qualified baseline"
        );
        let spec = specification(
            &store,
            &profile,
            &imported.directory,
            &prepared_path,
            &tools,
        );
        let index = BuildIndex::open(base.join("index"), store).unwrap();
        let prepared = index
            .publish_prepared_with_layout(
                &spec,
                &imported.directory,
                PreparedContentLayout::PagedChunkedV2,
            )
            .unwrap();
        index.store().abandon_staging(&staging.id).unwrap();
        (index, prepared, spec)
    };
    let preparation_elapsed_ms = started.elapsed().as_millis() as u64;
    assert!(
        index.prepared(&spec).unwrap().is_none(),
        "v2 preparation must not impersonate a legacy key"
    );
    assert_eq!(
        index
            .prepared_with_layout(&spec, PreparedContentLayout::PagedChunkedV2)
            .unwrap(),
        Some(prepared.clone())
    );
    assert!(index
        .store()
        .retained_closure(&prepared.receipt)
        .unwrap()
        .iter()
        .any(|reference| reference.kind == ObjectKind::File));
    let grpc = port();
    let mut http = port();
    while http == grpc {
        http = port();
    }
    let log = File::create(base.join("reapi-cache.log")).unwrap();
    let cache = Command::new(tools["bazel_remote"].as_str().unwrap())
        .args([
            "--dir",
            base.join("reapi-cache").to_str().unwrap(),
            "--max_size",
            "1",
            "--grpc_address",
            &format!("127.0.0.1:{grpc}"),
            "--http_address",
            &format!("127.0.0.1:{http}"),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let _cache = CacheProcess(cache);
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", grpc)).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    profile.buck2.remote_cache = Some(RemoteCacheConfig {
        cas_address: format!("grpc://127.0.0.1:{grpc}"),
        action_cache_address: format!("grpc://127.0.0.1:{grpc}"),
        instance_name: "rudder-runnable-v2-profile".into(),
        tls: false,
    });
    let binary = PathBuf::from(tools["buck2"].as_str().unwrap());
    let adapter = Buck2Adapter::new(&binary).unwrap();
    let mut daemons = Daemons {
        binary,
        workspaces: vec![],
    };
    let make_source = |rust: &str| {
        let lease = index
            .store()
            .begin_staging(Duration::from_secs(600))
            .unwrap();
        let path = base.join(format!("source-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("rust")).unwrap();
        fs::create_dir(path.join("typescript")).unwrap();
        fs::write(path.join("rust/lib.rs"), rust).unwrap();
        fs::write(
            path.join("typescript/main.ts"),
            include_str!("../../examples/build-profile/typescript/main.ts"),
        )
        .unwrap();
        fs::write(
            path.join("Cargo.lock"),
            include_str!("../../examples/build-profile/Cargo.lock"),
        )
        .unwrap();
        fs::write(
            path.join("package-lock.json"),
            include_str!("../../examples/build-profile/package-lock.json"),
        )
        .unwrap();
        let tree = tree::import_quiesced(
            index.store(),
            &lease.id,
            &path,
            &Default::default(),
            &Default::default(),
        )
        .unwrap();
        assert!(
            tree::validate_snapshot(index.store(), &tree, &Default::default())
                .unwrap()
                .groups
                .is_empty()
        );
        let policy = index
            .store()
            .put_blob(b"{\"format_version\":1,\"projection\":\"workspace_source_complement\"}")
            .unwrap();
        let source = snapshot::stage(
            index.store(),
            &lease.id,
            ObjectKind::SourceRoot,
            &SourceV2 {
                format_version: 2,
                consistency: CheckpointConsistency::Quiesced,
                directory: tree.directory.clone(),
                projection_policy: policy.clone(),
            },
        )
        .unwrap();
        index
            .store()
            .publish(
                &lease.id,
                &format!("workload-source/{}", source.digest),
                &source,
            )
            .unwrap();
        (source, tree.directory, policy)
    };
    let make_input = |source: &ObjectRef, source_dir: &ObjectRef, policy: &ObjectRef| {
        let lease = index
            .store()
            .begin_staging(Duration::from_secs(600))
            .unwrap();
        let environment =
            snapshot::decode_environment(&index.store().read_object(&prepared.root).unwrap())
                .unwrap();
        let mut entries = vec![];
        for directory in [source_dir, &environment.directory] {
            let mut after = None;
            loop {
                let page = tree::scan(index.store(), directory, after.as_deref(), 1024).unwrap();
                entries.extend(page.entries);
                after = page.next_after;
                if after.is_none() {
                    break;
                }
            }
        }
        let mut prepared_paths = vec![];
        let mut after = None;
        loop {
            let page = tree::scan(
                index.store(),
                &environment.directory,
                after.as_deref(),
                1024,
            )
            .unwrap();
            prepared_paths.extend(
                page.entries
                    .into_iter()
                    .map(|entry| vec![entry.name().unwrap()]),
            );
            after = page.next_after;
            if after.is_none() {
                break;
            }
        }
        if !prepared_paths.contains(&vec![b"node_modules".to_vec()]) {
            prepared_paths.push(vec![b"node_modules".to_vec()]);
        }
        let workspace_profile = WorkspaceProfile {
            format_version: 1,
            prepared: prepared_paths,
            scratch: vec![],
            vcs_local: vec![],
        };
        let root = PagedDirectory::new(&LocalDirectoryStore::new(index.store(), &lease.id))
            .build(0o755, entries)
            .unwrap();
        let projected = workspace::stage_source_complement(
            index.store(),
            &lease.id,
            &root,
            &workspace_profile,
            &ProjectionBudget::default(),
        )
        .unwrap();
        assert_eq!(projected.directory, *source_dir);
        let profile_ref = index
            .store()
            .put_blob(&canonical_json(&workspace_profile).unwrap())
            .unwrap();
        let hardlinks = index
            .store()
            .put_blob(b"{\"format_version\":1,\"groups\":[]}")
            .unwrap();
        let runnable = snapshot::stage(
            index.store(),
            &lease.id,
            ObjectKind::RunnableRoot,
            &RunnableRoot {
                format_version: 1,
                consistency: CheckpointConsistency::Quiesced,
                source: source.clone(),
                environment: Some(prepared.root.clone()),
                view_root: root,
                profile: profile_ref,
                hardlinks,
                knowledge_cursor: 0,
            },
        )
        .unwrap();
        let publication_started = Instant::now();
        index
            .store()
            .publish_extending(
                &lease.id,
                &format!("workload-runnable/{}", runnable.digest),
                &runnable,
                &[source.clone(), prepared.receipt.clone()],
            )
            .unwrap();
        let publication_ms = publication_started.elapsed().as_millis() as u64;
        let input = workspace::runnable_build_input_view(
            index.store(),
            &runnable,
            policy,
            &ProjectionBudget::default(),
        )
        .unwrap();
        (input, publication_ms, projected.report)
    };
    let source = make_source(include_str!("../../examples/build-profile/rust/lib.rs"));
    let edited =
        make_source(&include_str!("../../examples/build-profile/rust/lib.rs").replace("42", "43"));
    let mut cases = vec![];
    let mut outputs = None;
    let mut initial_digests = None;
    for (name, input_source, view_name, executed, hits, graph_reused) in [
        ("cold", &source, "first-view", 2, 0, 0),
        ("warm-same-graph", &source, "first-view", 0, 0, 2),
        ("warm-same-graph-2", &source, "first-view", 0, 0, 2),
        ("warm-same-graph-3", &source, "first-view", 0, 0, 2),
        ("fresh-graph-reapi-hit", &source, "fresh-view", 0, 2, 0),
        ("source-edit", &edited, "edited-view", 1, 1, 0),
    ] {
        let path = base.join(view_name);
        if !daemons.workspaces.contains(&path) {
            daemons.workspaces.push(path.clone());
        }
        let started = Instant::now();
        let (input, publication_ms, projection) =
            make_input(&input_source.0, &input_source.1, &input_source.2);
        let input_ms = started.elapsed().as_millis() as u64;
        assert!(!input.input().dirty_prepared());
        let run = adapter
            .build(&index, &profile, input.input(), &prepared, &path)
            .unwrap();
        let wall_ms = started.elapsed().as_millis() as u64;
        assert_eq!(run.observation.executed_actions, executed);
        assert_eq!(run.observation.remote_cache_hits, hits);
        assert_eq!(run.frontend_graph_reused_actions, graph_reused);
        if name == "cold" {
            outputs = Some(run.stored.outputs.clone());
            initial_digests = Some(run.observation.native_action_digests.clone());
        } else if name != "source-edit" {
            assert_eq!(Some(&run.stored.outputs), outputs.as_ref());
            if name == "fresh-graph-reapi-hit" {
                assert_eq!(
                    Some(&run.observation.native_action_digests),
                    initial_digests.as_ref()
                );
            }
        } else {
            assert_ne!(Some(&run.stored.outputs), outputs.as_ref());
        }
        record(&base, name, &index, &run, &mut cases, wall_ms);
        let case = cases.last_mut().unwrap();
        case["input_construction_ms"] = json!(input_ms);
        case["runnable_publication_ms"] = json!(publication_ms);
        case["source_projection"] = json!(projection);
        case["input_qualification"] = json!(input.report());
        input.release().unwrap();
        if !["cold", "warm-same-graph", "warm-same-graph-2"].contains(&name) {
            adapter.stop(&path).unwrap();
        }
    }
    let smoke = base.join("artifact-smoke");
    index
        .store()
        .export_tree(&outputs.unwrap(), &smoke)
        .unwrap();
    let rust_bytes = fs::read(smoke.join("rust/rust.rlib")).unwrap();
    assert!(rust_bytes.starts_with(b"!<arch>\n") && rust_bytes.len() > 1024);
    let output = Command::new(prepared_path.join("tools/node"))
        .args([
            "-e",
            "process.stdout.write(String(require(process.argv[1]).answer))",
            smoke.join("typescript/main.js").to_str().unwrap(),
        ])
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"42");
    let summary = json!({"format_version":1,"verified_at_epoch_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64,"layout":"runnable-root-v1/source-v2/environment-v2/directory-v2/file-v1","preparation_mode":if reuse.is_some(){"retained-cas-index-reuse"}else{"fresh-import-and-publication"},"preparation_elapsed_ms":preparation_elapsed_ms,"cases":cases,"artifact_smoke":{"rust_archive_valid":true,"typescript_answer":42},"scope":{"materialization":"eager streamed and fully reverified","linked_inputs_qualified":false,"remote_execution":false,"cache_protocol":"native-gRPC-REAPI-v2","cold_definition":"empty frontend graph and REAPI action cache; existing CAS/index reuse explicitly recorded; OS page cache not flushed","rust_workload":"rlib; no binary linker claim","typescript_workload":"compiled JavaScript executed with prepared Node"}});
    fs::write(
        base.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    println!("{}", serde_json::to_string(&summary).unwrap());
}
