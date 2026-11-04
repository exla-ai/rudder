//! Actual local worker processes, sharing immutable CAS and preparation receipts.
//! This qualifies the worker/data plane without model inference, a mount, or a
//! frontend action-cache claim. Host cc executes a deterministic fixture in
//! private scratch; it is not the pinned managed build profile.
use rudder_engine::{
    build::{BuildEffect, BuildIndex, PlatformIdentity, PreparationSpec, ToolchainIdentity},
    directory::{
        DirectoryEntry as PagedEntry, EntryNode as PagedNode, LocalDirectoryStore, PagedDirectory,
    },
    journal::{
        CapabilityGrant, IntegrationEvidence, IntegrationRequest, Journal, MessageIntent,
        MessagePriority, MessageRequest, OwnerFence,
    },
    protocol::{
        canonical_json, CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch, FORMAT_VERSION,
    },
    store::{DirectoryEntry, DirectoryManifest, EntryNode, LocalStore, SourceManifest},
    vfs::{NodeKind, VfsManager},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
struct Input {
    spec: PreparationSpec,
    directory: ObjectRef,
    source: ObjectRef,
    source_root: ObjectRef,
    owner: OwnerFence,
}
#[derive(Serialize, Deserialize, Debug)]
struct Report {
    pid: u32,
    initial_hit: bool,
    produced: bool,
    prepared_root: ObjectRef,
    view_id: String,
    namespace: String,
    immutable_source: Vec<u8>,
    private_source: Vec<u8>,
    private_git: Vec<u8>,
    private_scratch: Vec<u8>,
    compiler_output: ObjectRef,
    execution_stdout: Vec<u8>,
    mailbox_acknowledged: bool,
    integrated: bool,
    elapsed_ns: u128,
}
struct Fixture {
    temp: tempfile::TempDir,
    input: Input,
}
impl Fixture {
    fn new() -> Self {
        let base = if Path::new("/workspace").is_dir() {
            PathBuf::from("/workspace")
        } else {
            std::env::temp_dir()
        };
        let temp = tempfile::Builder::new()
            .prefix("rudder-multiprocess-cache-")
            .tempdir_in(base)
            .unwrap();
        let store = LocalStore::open(temp.path().join("cas")).unwrap();
        let compiler = store.put_blob(b"pinned compiler identity fixture").unwrap();
        let tools = directory(&store, vec![file_entry(b"compiler", compiler.clone())]);
        let environment = directory(&store, vec![dir_entry(b"tools", tools)]);
        let source_file = store.put_blob(b"#include <stdio.h>\nint main(void) { puts(\"shared immutable source\"); return 0; }\n").unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let adapter = LocalDirectoryStore::new(&store, &stage.id);
        let source = PagedDirectory::new(&adapter)
            .build(
                0o755,
                vec![PagedEntry::new(
                    b"source",
                    PagedNode::File {
                        object: source_file.clone(),
                        logical_size: source_file.size_bytes,
                        mode: 0o644,
                    },
                )
                .unwrap()],
            )
            .unwrap();
        store
            .publish(&stage.id, "multi-process-paged-source", &source)
            .unwrap();
        store.pin("multi-process-source", &source).unwrap();
        store
            .pin("multi-process-environment", &environment)
            .unwrap();
        let lockfile = store.put_blob(b"dependency-lock-v1").unwrap();
        let source_root = store
            .put_source(&SourceManifest {
                format_version: 1,
                directory: source.clone(),
                parents: vec![],
                consistency: CheckpointConsistency::Quiesced,
            })
            .unwrap();
        store
            .pin("multi-process-source-root", &source_root)
            .unwrap();
        let mut journal = Journal::open(temp.path().join("journal.sqlite")).unwrap();
        let owner = journal
            .create_project(
                "qualification",
                "qualification-serial-integrator",
                "base",
                1000,
            )
            .unwrap();
        let mut tokens = BTreeMap::new();
        for id in ["left", "right"] {
            journal.register_run(&owner, id, 1000).unwrap();
        }
        for id in ["left", "right"] {
            let grant = CapabilityGrant {
                principal: format!("worker-{id}"),
                run_id: id.into(),
                run_epoch: 1,
                operations: ["message:send", "mailbox:read", "mailbox:ack"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                recipients: BTreeMap::from([("left".into(), 1), ("right".into(), 1)]),
                scopes: BTreeSet::from(["project".into()]),
                topics: BTreeSet::new(),
                expires_at_ms: None,
            };
            tokens.insert(
                id.to_owned(),
                journal.issue_capability(&owner, &grant, 1000).unwrap(),
            );
        }
        // Ephemeral run capabilities stay in this private disposable fixture.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(
            temp.path().join("credentials.json"),
            canonical_json(&tokens).unwrap(),
        )
        .unwrap();
        let input = Input {
            spec: PreparationSpec {
                format_version: FORMAT_VERSION,
                platform: PlatformIdentity {
                    os: std::env::consts::OS.into(),
                    architecture: std::env::consts::ARCH.into(),
                    abi: "qualification-v1".into(),
                    properties: BTreeMap::new(),
                },
                base_runtime: environment.clone(),
                installer_inputs: BTreeMap::from([("dependency.lock".into(), lockfile)]),
                installer_commands: vec![vec!["pinned-preparation-fixture".into()]],
                installer_environment: BTreeMap::new(),
                toolchains: BTreeMap::from([(
                    "compiler".into(),
                    ToolchainIdentity {
                        version: "1".into(),
                        closure: environment.clone(),
                        executable: compiler,
                        executable_path: "tools/compiler".into(),
                        configuration: BTreeMap::new(),
                    },
                )]),
                policy_version: "qualification-v1".into(),
                effects: BTreeSet::new(),
            },
            directory: environment,
            source,
            source_root,
            owner,
        };
        fs::write(
            temp.path().join("input.json"),
            canonical_json(&input).unwrap(),
        )
        .unwrap();
        // Initialize shared database schemas before the independently executing
        // workers race on a cold key, rather than racing schema creation itself.
        drop(BuildIndex::open(temp.path().join("index"), store.clone()).unwrap());
        let manager = VfsManager::open(temp.path().join("manager"), store).unwrap();
        manager.prepare_paged(&input.source).unwrap();
        Self { temp, input }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn index(&self) -> BuildIndex {
        BuildIndex::open(
            self.root().join("index"),
            LocalStore::open(self.root().join("cas")).unwrap(),
        )
        .unwrap()
    }
    fn spawn(&self, id: &str, mode: &str) -> Worker {
        let stdout = File::create(self.root().join(format!("{id}.stdout"))).unwrap();
        let stderr = File::create(self.root().join(format!("{id}.stderr"))).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worker_process", "--nocapture"])
            .env("RUDDER_MULTIPROCESS_FIXTURE", self.root())
            .env("RUDDER_MULTIPROCESS_ID", id)
            .env("RUDDER_MULTIPROCESS_MODE", mode)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap();
        Worker {
            child,
            root: self.root().into(),
            id: id.into(),
            settled: false,
        }
    }
    fn release(&self) {
        fs::write(self.root().join("start"), b"go").unwrap();
    }
    fn callbacks(&self) -> Vec<String> {
        fs::read_to_string(self.root().join("callbacks"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}
struct Worker {
    child: Child,
    root: PathBuf,
    id: String,
    settled: bool,
}
impl Worker {
    fn finish(mut self) -> Report {
        let end = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < end, "worker {} did not finish", self.id);
            std::thread::sleep(Duration::from_millis(5));
        };
        self.settled = true;
        assert!(
            status.success(),
            "worker {} failed: {} {}",
            self.id,
            fs::read_to_string(self.root.join(format!("{}.stdout", self.id))).unwrap(),
            fs::read_to_string(self.root.join(format!("{}.stderr", self.id))).unwrap()
        );
        serde_json::from_slice(&fs::read(self.root.join(format!("{}.report", self.id))).unwrap())
            .unwrap()
    }
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.settled = true;
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn until_file(path: &Path) {
    let end = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < end,
            "missing worker boundary {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn directory(store: &LocalStore, entries: Vec<DirectoryEntry>) -> ObjectRef {
    store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries,
        })
        .unwrap()
}
fn file_entry(name: &[u8], object: ObjectRef) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryNode::File {
            object,
            mode: 0o644,
        },
    )
    .unwrap()
}
fn dir_entry(name: &[u8], object: ObjectRef) -> DirectoryEntry {
    DirectoryEntry::new(name, EntryNode::Directory { object }).unwrap()
}

/// Child entrypoint is a normal test so Cargo's exact current test binary owns
/// both parent and children. No ignored executable or stale helper is accepted.
#[test]
fn worker_process() {
    let Some(root) = std::env::var_os("RUDDER_MULTIPROCESS_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let id = std::env::var("RUDDER_MULTIPROCESS_ID").unwrap();
    let mode = std::env::var("RUDDER_MULTIPROCESS_MODE").unwrap();
    let input: Input = serde_json::from_slice(&fs::read(root.join("input.json")).unwrap()).unwrap();
    let store = LocalStore::open(root.join("cas")).unwrap();
    let index = BuildIndex::open(root.join("index"), store.clone()).unwrap();
    let initial_hit = index.prepared(&input.spec).unwrap().is_some();
    fs::write(root.join(format!("{id}.ready")), b"ready").unwrap();
    until_file(&root.join("start"));
    let began = Instant::now();
    let mut produced = false;
    let prepared = index.prepare_once(&input.spec, |_| {
        produced = true;
        let mut callbacks = OpenOptions::new()
            .append(true)
            .create(true)
            .open(root.join("callbacks"))?;
        writeln!(callbacks, "{id}")?;
        callbacks.sync_all()?;
        if mode == "cancel" {
            fs::write(
                root.join(format!("{id}.producing")),
                b"original producer owns the miss",
            )?;
            until_file(&root.join("never-release-cancelled-producer"));
        }
        if mode == "fail" {
            anyhow::bail!("qualification producer failed before publication");
        }
        Ok(input.directory.clone())
    });
    if mode == "fail" {
        assert!(prepared.is_err());
        assert!(index.prepared(&input.spec).unwrap().is_none());
        return;
    }
    let prepared = prepared.unwrap();
    let manager = VfsManager::open(root.join("manager"), store).unwrap();
    let view_id = uuid::Uuid::new_v4().to_string();
    let owner = OwnerEpoch {
        owner: "qualification-serial-integrator".into(),
        epoch: 1,
    };
    let view = manager.fork_paged(&view_id, &input.source, owner).unwrap();
    let source = view.lookup(1, b"source").unwrap();
    let immutable_source = view.read(source.ino, 0, 1024).unwrap();
    let private_bytes = format!("private state from {id}");
    view.write(source.ino, 0, private_bytes.as_bytes(), true)
        .unwrap();
    let git = view
        .create(1, b".git", NodeKind::Directory, 0o700, b"")
        .unwrap();
    let git_index = view
        .create(git.ino, b"index", NodeKind::File, 0o600, b"")
        .unwrap();
    view.write(git_index.ino, 0, private_bytes.as_bytes(), true)
        .unwrap();
    let target = view
        .create(1, b"target", NodeKind::Directory, 0o700, b"")
        .unwrap();
    let scratch = view
        .create(target.ino, b"compiler-scratch", NodeKind::File, 0o600, b"")
        .unwrap();
    view.write(scratch.ino, 0, private_bytes.as_bytes(), true)
        .unwrap();
    let scratch_path = root.join(format!("worker-{id}"));
    fs::create_dir(&scratch_path).unwrap();
    fs::create_dir(scratch_path.join("target")).unwrap();
    fs::write(scratch_path.join("fixture.c"), &immutable_source).unwrap();
    let compiled = Command::new("/usr/bin/cc")
        // GNU ld only; the macOS linker rejects --build-id.
        .args(if cfg!(target_os = "linux") { &["-Wl,--build-id=none"][..] } else { &[][..] })
        .args([
            "-frandom-seed=rudder-fixture",
            "-fno-ident",
            "fixture.c",
            "-o",
            "target/fixture",
        ])
        .current_dir(&scratch_path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("SOURCE_DATE_EPOCH", "0")
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "fixture cc failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let executed = Command::new(scratch_path.join("target/fixture"))
        .env_clear()
        .output()
        .unwrap();
    assert!(executed.status.success());
    let compiler_output = ObjectRef::from_bytes(
        ObjectKind::Blob,
        &fs::read(scratch_path.join("target/fixture")).unwrap(),
    );
    let mut mailbox_acknowledged = false;
    let mut integrated = false;
    if id == "left" || id == "right" {
        let peer = if id == "left" { "right" } else { "left" };
        let tokens: BTreeMap<String, String> =
            serde_json::from_slice(&fs::read(root.join("credentials.json")).unwrap()).unwrap();
        let token = &tokens[&id];
        let mut journal = Journal::open(root.join("journal.sqlite")).unwrap();
        journal
            .send_message(
                token,
                &MessageRequest {
                    idempotency_key: format!("compiled-{id}"),
                    recipient_run: peer.into(),
                    recipient_epoch: 1,
                    scope: "project".into(),
                    intent: MessageIntent::Inform,
                    body: serde_json::json!({"compiler_output":compiler_output}),
                    expires_at_ms: 10000,
                    priority: MessagePriority::Normal,
                    conversation_id: None,
                    request_id: None,
                    reply_to: None,
                    supersedes: None,
                    topic_versions: BTreeMap::new(),
                    artifacts: vec![],
                },
                1000,
            )
            .unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let messages = journal.inbox(token, 10, 1000).unwrap();
            if let Some(message) = messages.first() {
                assert_eq!(message.sender_run, peer);
                assert_eq!(
                    message.request.body["compiler_output"],
                    serde_json::to_value(&compiler_output).unwrap()
                );
                let cursor = journal
                    .ack_message(token, &message.message_id, 1000)
                    .unwrap();
                assert!(cursor.sequence >= message.sequence);
                mailbox_acknowledged = true;
                break;
            }
            assert!(
                Instant::now() < end,
                "peer worker never delivered compilation message"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let request = IntegrationRequest {
            idempotency_key: format!("integrate-{id}"),
            expected_head: "base".into(),
            new_head: format!("accepted-{id}"),
            proposed_source_root: input.source_root.clone(),
            proposed_environment_root: prepared.root.clone(),
            versions: BTreeMap::new(),
            evidence: IntegrationEvidence {
                tested_head: "base".into(),
                tested_source_root: input.source_root.clone(),
                tested_environment_root: prepared.root.clone(),
                action_results: vec![],
                checks: serde_json::json!({"scope":"local compiler fixture only", "compiler_output":compiler_output,"exit_code":0}),
            },
        };
        integrated = journal.integrate(&input.owner, &request, 1000).is_ok();
    }
    let report = Report {
        pid: std::process::id(),
        initial_hit,
        produced,
        prepared_root: prepared.root,
        view_id,
        namespace: view.dirty_cursor().unwrap().namespace,
        immutable_source,
        private_source: view.read(source.ino, 0, 1024).unwrap(),
        private_git: view.read(git_index.ino, 0, 1024).unwrap(),
        private_scratch: view.read(scratch.ino, 0, 1024).unwrap(),
        compiler_output,
        execution_stdout: executed.stdout,
        mailbox_acknowledged,
        integrated,
        elapsed_ns: began.elapsed().as_nanos(),
    };
    fs::write(
        root.join(format!("{id}.report")),
        canonical_json(&report).unwrap(),
    )
    .unwrap();
}

#[test]
fn real_processes_coalesce_cold_preparation_and_keep_source_git_and_scratch_private() {
    let f = Fixture::new();
    let left = f.spawn("left", "normal");
    let right = f.spawn("right", "normal");
    until_file(&f.root().join("left.ready"));
    until_file(&f.root().join("right.ready"));
    f.release();
    let left = left.finish();
    let right = right.finish();
    assert_ne!(left.pid, right.pid);
    assert_ne!(left.pid, std::process::id());
    assert!(!left.initial_hit && !right.initial_hit);
    assert_eq!(usize::from(left.produced) + usize::from(right.produced), 1);
    assert_eq!(f.callbacks().len(), 1);
    assert_eq!(left.prepared_root, right.prepared_root);
    assert_eq!(left.immutable_source, right.immutable_source);
    assert_eq!(left.compiler_output, right.compiler_output);
    assert_eq!(left.execution_stdout, b"shared immutable source\n");
    assert_eq!(right.execution_stdout, left.execution_stdout);
    assert!(left.mailbox_acknowledged && right.mailbox_acknowledged);
    assert_eq!(
        usize::from(left.integrated) + usize::from(right.integrated),
        1
    );
    assert_ne!(left.view_id, right.view_id);
    assert_ne!(left.namespace, right.namespace);
    assert_ne!(left.private_source, right.private_source);
    assert_eq!(left.private_git, b"private state from left");
    assert_eq!(right.private_git, b"private state from right");
    assert_eq!(left.private_scratch, left.private_git);
    assert_eq!(right.private_scratch, right.private_git);
    let warm = f.spawn("warm", "normal").finish();
    assert!(warm.initial_hit && !warm.produced);
    assert_eq!(f.callbacks().len(), 1);
    assert_eq!(warm.immutable_source, left.immutable_source);
    println!(
        "RUDDER_MULTI_PROCESS_CACHE_V1={}",
        serde_json::json!({"scope":"real local VFS worker processes, host cc fixture, durable mailbox acknowledgment, journal integration fence and preparation index; no model inference, mount, managed compiler or frontend action-cache claim", "cold_workers":[left,right],"warm_worker":warm,"preparation_callbacks":1,"integration_owner":"qualification-serial-integrator:1"})
    );
}

#[test]
fn killed_and_failed_real_producers_cannot_publish_and_poisoned_receipt_cannot_hit() {
    let f = Fixture::new();
    let cancelled = f.spawn("cancelled", "cancel");
    until_file(&f.root().join("cancelled.ready"));
    f.release();
    until_file(&f.root().join("cancelled.producing"));
    assert!(f.index().prepared(&f.input.spec).unwrap().is_none());
    cancelled.kill();
    assert!(f.index().prepared(&f.input.spec).unwrap().is_none());
    let failed = f.spawn("failed", "fail");
    let mut failed = failed;
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = failed.child.try_wait().unwrap() {
            assert!(status.success());
            failed.settled = true;
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(f.index().prepared(&f.input.spec).unwrap().is_none());
    let recovered = f.spawn("recovered", "normal").finish();
    assert!(recovered.produced && !recovered.initial_hit);
    assert_eq!(f.callbacks(), ["cancelled", "failed", "recovered"]);
    let index = f.index();
    let prepared = index.prepared(&f.input.spec).unwrap().unwrap();
    let path = index.store().object_path(&prepared.receipt).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(path, vec![b'?'; prepared.receipt.size_bytes as usize]).unwrap();
    assert!(
        index.prepared(&f.input.spec).is_err(),
        "a poisoned receipt must fail digest verification"
    );
}

#[test]
fn bounded_preparation_invalidation_matrix_keeps_environment_and_source_identity_separate() {
    let f = Fixture::new();
    let index = f.index();
    let base = index
        .publish_prepared(&f.input.spec, &f.input.directory)
        .unwrap();
    let unchanged = f.input.spec.key(index.store()).unwrap();
    index
        .store()
        .put_blob(b"changed application source")
        .unwrap();
    assert_eq!(f.input.spec.key(index.store()).unwrap(), unchanged);
    let mut cases = Vec::new();
    for dimension in [
        "dependency",
        "sdk",
        "flags",
        "platform",
        "environment",
        "command",
        "policy",
    ] {
        let mut spec = f.input.spec.clone();
        match dimension {
            "dependency" => {
                spec.installer_inputs.insert(
                    "dependency.lock".into(),
                    index.store().put_blob(b"dependency-lock-v2").unwrap(),
                );
            }
            "sdk" => {
                spec.toolchains.get_mut("compiler").unwrap().version = "2".into();
            }
            "flags" => {
                spec.toolchains
                    .get_mut("compiler")
                    .unwrap()
                    .configuration
                    .insert("flags".into(), "-O2".into());
            }
            "platform" => spec.platform.abi = "qualification-v2".into(),
            "environment" => {
                spec.installer_environment.insert("LANG".into(), "C".into());
            }
            "command" => spec.installer_commands[0].push("--different".into()),
            "policy" => spec.policy_version = "qualification-v2".into(),
            _ => unreachable!(),
        }
        let started = Instant::now();
        assert_ne!(spec.key(index.store()).unwrap(), base.key);
        assert!(index.prepared(&spec).unwrap().is_none());
        cases.push(serde_json::json!({"dimension":dimension,"hit":false,"ns":started.elapsed().as_nanos()}));
    }
    for effect in [
        BuildEffect::Network,
        BuildEffect::Clock,
        BuildEffect::Randomness,
        BuildEffect::Secrets,
    ] {
        let mut spec = f.input.spec.clone();
        spec.effects.insert(effect);
        assert!(
            index.prepared(&spec).is_err(),
            "unmodeled effects cannot authorize a reusable preparation"
        );
    }
    assert_eq!(index.prepared(&f.input.spec).unwrap(), Some(base));
    println!(
        "RUDDER_PREPARATION_INVALIDATION_V1={}",
        serde_json::json!({"scope":"preparation identity and receipt lookup; source/action invalidation belongs to frontend qualification", "source_change_preserves_environment_key":true,"matrix":cases})
    );
}
