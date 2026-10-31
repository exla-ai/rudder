use super::*;
use rudder_engine::{
    protocol::{ObjectKind, ObjectRef, OwnerEpoch},
    store::ImportPolicy,
    vfs::{
        exclusion_policy_digest,
        rpc::{AttemptContext, ObservationBinding},
    },
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, AtomicUsize},
};

struct TemporaryDirectory(PathBuf);
impl TemporaryDirectory {
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn temporary_in(parent: &Path) -> std::io::Result<TemporaryDirectory> {
    use std::os::unix::fs::DirBuilderExt;
    let path = parent
        .canonicalize()?
        .join(format!("rudder-remote-factory-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    Ok(TemporaryDirectory(path))
}
fn temporary() -> std::io::Result<TemporaryDirectory> {
    temporary_in(&std::env::temp_dir())
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn binding() -> PartialBinding {
    let attempt = AttemptBinding {
        project_id: "p".into(),
        run_id: "r".into(),
        run_epoch: 1,
        owner: OwnerEpoch {
            owner: "owner".into(),
            epoch: 1,
        },
        execution_id: uuid(),
        operation_id: uuid(),
        view_id: uuid(),
    };
    PartialBinding {
        protocol: PROTOCOL.into(),
        format_version: 1,
        launch_request_digest: "a".repeat(64),
        root: RootBinding {
            scope: CacheScope {
                provider_id: "fixture".into(),
                tenant_id: uuid(),
            },
            root_id: "root".into(),
            root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"fixture"),
        },
        observation: ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: attempt.execution_id.clone(),
                operation_id: attempt.operation_id.clone(),
            },
            view_id: attempt.view_id.clone(),
            owner: attempt.owner.clone(),
            namespace: uuid(),
            profile: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture profile"),
            exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
        },
        attempt,
    }
}
struct Broker {
    calls: AtomicUsize,
    deny: AtomicBool,
    advance: Mutex<Option<(Arc<AtomicU64>, u64)>>,
}
impl BrokerAuthorization for Broker {
    fn authorize(&self, _: &PartialBinding) -> Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some((clock, value)) = self.advance.lock().unwrap().take() {
            clock.store(value, Ordering::Release);
        }
        ensure!(
            !self.deny.load(Ordering::Acquire),
            "secret broker detail must not escape"
        );
        Ok(())
    }
}
fn fixture_authority() -> (
    Arc<PartialAuthority>,
    Arc<AtomicU64>,
    Arc<SessionTracker>,
    Arc<Broker>,
) {
    let clock = Arc::new(AtomicU64::new(1));
    let time = clock.clone();
    let tracker = Arc::new(SessionTracker(Mutex::new(None)));
    let state = AuthorityState::new(
        binding(),
        Arc::new(move || Ok(time.load(Ordering::Acquire) as u128)),
        &tracker,
    );
    let broker = Arc::new(Broker {
        calls: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
        advance: Mutex::new(None),
    });
    (
        Arc::new(PartialAuthority {
            broker: broker.clone(),
            state,
        }),
        clock,
        tracker,
        broker,
    )
}
#[test]
fn remote_freshness_is_initialized_by_current_authorization_and_expires_stickily() {
    let (authority, clock, _tracker, broker) = fixture_authority();
    // A root checker must not authorize an uninitialized mirrored state.
    let other = AuthorityState::new(
        authority.state.binding.clone(),
        Arc::new(|| Ok(1)),
        &Arc::new(SessionTracker(Mutex::new(None))),
    );
    assert!(other.check().is_err());
    authority.validate(&authority.state.binding).unwrap();
    assert!(authority.state.check().is_ok());
    clock.store(FRESHNESS_NS as u64 + 1, Ordering::Release);
    assert!(authority.cancelled());
    clock.store(2, Ordering::Release);
    assert!(authority.validate(&authority.state.binding).is_err());
    assert_eq!(
        broker.calls.load(Ordering::Acquire),
        1,
        "new auth request cannot revive expired state"
    );
}
#[test]
fn remote_broker_failure_foreign_binding_and_clock_regression_are_permanent_static_fences() {
    for failure in [0, 1, 2] {
        let (authority, clock, _tracker, broker) = fixture_authority();
        authority.validate(&authority.state.binding).unwrap();
        match failure {
            0 => broker.deny.store(true, Ordering::Release),
            1 => clock.store(0, Ordering::Release),
            _ => {}
        }
        let mut offered = authority.state.binding.clone();
        if failure == 2 {
            offered.launch_request_digest = "f".repeat(64);
        }
        let error = authority.validate(&offered).unwrap_err();
        assert_eq!(
            error.to_string(),
            "remote workspace authorization unavailable"
        );
        broker.deny.store(false, Ordering::Release);
        clock.store(2, Ordering::Release);
        assert!(authority.validate(&authority.state.binding).is_err());
    }
}
#[test]
fn remote_late_positive_and_out_of_order_responses_do_not_extend_permission() {
    let (authority, clock, _tracker, broker) = fixture_authority();
    *broker.advance.lock().unwrap() = Some((clock.clone(), FRESHNESS_NS as u64 + 1));
    assert!(authority.validate(&authority.state.binding).is_err());
    assert!(authority.state.fenced.load(Ordering::Acquire));
    let (authority, clock, _tracker, _broker) = fixture_authority();
    let first = authority.state.begin().unwrap();
    clock.store(100, Ordering::Release);
    let newer = authority.state.begin().unwrap();
    authority.state.accept(newer).unwrap();
    let until = authority.state.freshness.lock().unwrap().until;
    clock.store(200, Ordering::Release);
    authority.state.accept(first).unwrap();
    assert_eq!(authority.state.freshness.lock().unwrap().until, until);
}
#[test]
fn root_authority_is_memory_only_and_requires_exact_root_attempt_without_broker_io() {
    let (authority, _clock, _tracker, broker) = fixture_authority();
    authority.validate(&authority.state.binding).unwrap();
    let checks = broker.calls.load(Ordering::Acquire);
    let root = RootAuthority(authority.state.clone());
    for _ in 0..100 {
        root.validate(
            &authority.state.binding.attempt,
            &authority.state.binding.root,
        )
        .unwrap();
    }
    assert_eq!(broker.calls.load(Ordering::Acquire), checks);
    let mut wrong = authority.state.binding.root.clone();
    wrong.root_id = "foreign".into();
    assert!(root
        .validate(&authority.state.binding.attempt, &wrong)
        .is_err());
    assert!(root
        .validate(
            &authority.state.binding.attempt,
            &authority.state.binding.root
        )
        .is_err());
}
#[test]
fn clock_failure_and_serial_overflow_fence_without_touching_broker() {
    let tracker = Arc::new(SessionTracker(Mutex::new(None)));
    let state = AuthorityState::new(binding(), Arc::new(|| Err(fault())), &tracker);
    assert!(state.begin().is_err());
    assert!(state.fenced.load(Ordering::Acquire));
    let (authority, _clock, _tracker, broker) = fixture_authority();
    authority.state.freshness.lock().unwrap().next = u64::MAX;
    assert!(authority.validate(&authority.state.binding).is_err());
    assert_eq!(broker.calls.load(Ordering::Acquire), 0);
}
#[test]
fn runtime_pin_checks_actual_content_identity_replacement_and_symlink_before_startup() {
    let base = temporary().unwrap();
    let file = base.path().join("node");
    fs::write(&file, b"\x7fELFowned fixture").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o500)).unwrap();
    let reference = RuntimeFile {
        path: file.clone(),
        sha256: hex_digest(fs::read(&file).unwrap()),
        size_bytes: fs::metadata(&file).unwrap().len(),
    };
    let mut pin = PinnedRuntime::open(&reference, true, RuntimeTrust::Fixture).unwrap();
    let original = base.path().join("original");
    fs::rename(&file, &original).unwrap();
    fs::write(&file, b"\x7fELFowned fixture").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        pin.verify().is_err(),
        "matching bytes do not replace a held inode"
    );
    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&original, &file).unwrap();
    assert!(PinnedRuntime::open(&reference, true, RuntimeTrust::Fixture).is_err());
    let fifo = base.path().join("fifo");
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let reference = RuntimeFile {
        path: fifo,
        sha256: "a".repeat(64),
        size_bytes: 1,
    };
    assert!(PinnedRuntime::open(&reference, false, RuntimeTrust::Fixture).is_err());
}
#[test]
fn production_runtime_rejects_uid_writable_ancestors_even_with_correct_file_hash() {
    let base = temporary().unwrap();
    fs::set_permissions(base.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let file = base.path().join("cli.mjs");
    fs::write(&file, b"// fixture").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).unwrap();
    let reference = RuntimeFile {
        path: file,
        sha256: hex_digest(b"// fixture"),
        size_bytes: 10,
    };
    assert!(PinnedRuntime::open(&reference, false, RuntimeTrust::Image).is_err());
}

struct Graph(Mutex<BTreeMap<rudder_engine::protocol::ObjectRef, Vec<u8>>>);
impl Graph {
    fn put(
        &self,
        kind: rudder_engine::protocol::ObjectKind,
        bytes: &[u8],
    ) -> rudder_engine::protocol::ObjectRef {
        let reference = rudder_engine::protocol::ObjectRef::from_bytes(kind, bytes);
        self.0
            .lock()
            .unwrap()
            .insert(reference.clone(), bytes.into());
        reference
    }
    fn metadata(
        &self,
        kind: rudder_engine::protocol::ObjectKind,
        value: &impl serde::Serialize,
    ) -> rudder_engine::protocol::ObjectRef {
        self.put(kind, &canonical_json(value).unwrap())
    }
}
impl rudder_engine::directory::DirectoryStore for Graph {
    fn read(&self, reference: &rudder_engine::protocol::ObjectRef) -> Result<Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .context("fixture metadata missing")
    }
    fn put(
        &self,
        kind: rudder_engine::protocol::ObjectKind,
        bytes: &[u8],
    ) -> Result<rudder_engine::protocol::ObjectRef> {
        Ok(Graph::put(self, kind, bytes))
    }
}
fn graph() -> (
    Arc<Graph>,
    RemoteLaunchRequest,
    rudder_engine::remote_source::ReplicatedRootReceipt,
) {
    use rudder_engine::{
        directory::PagedDirectory,
        protocol::CheckpointConsistency,
        snapshot::{RunnableRoot, SourceV2},
        workspace::WorkspaceProfile,
    };
    let graph = Arc::new(Graph(Mutex::new(BTreeMap::new())));
    let directory = PagedDirectory::new(graph.as_ref())
        .build(0o755, [])
        .unwrap();
    let seed = PagedDirectory::new(graph.as_ref())
        .build(0o700, [])
        .unwrap();
    let profile = graph.metadata(ObjectKind::Blob, &WorkspaceProfile::source_only());
    let policy = graph.put(ObjectKind::Blob, b"{}");
    let source = graph.metadata(
        ObjectKind::SourceRoot,
        &SourceV2 {
            format_version: 2,
            consistency: CheckpointConsistency::Quiesced,
            directory: directory.clone(),
            projection_policy: policy.clone(),
        },
    );
    let hardlinks = graph.metadata(
        ObjectKind::Blob,
        &serde_json::json!({"format_version":1,"groups":[]}),
    );
    let root = graph.metadata(
        ObjectKind::RunnableRoot,
        &RunnableRoot {
            format_version: 1,
            consistency: CheckpointConsistency::Quiesced,
            source: source.clone(),
            environment: None,
            view_root: directory,
            profile: profile.clone(),
            hardlinks,
            knowledge_cursor: 0,
        },
    );
    let mut expected = binding();
    expected.root.scope = CacheScope {
        provider_id: "native-factory-fixture".into(),
        tenant_id: uuid(),
    };
    expected.root.root = root.clone();
    expected.observation.profile = profile;
    let selection = RemoteLaunchRequest {
        format_version: 1,
        request_id: uuid(),
        root: expected.root.clone(),
        attempt: expected.attempt,
        observation: expected.observation,
        projection_policy: policy,
        accepted_source: source,
        accepted_seed: seed,
    };
    let (object_count, total_bytes) = {
        let objects = graph.0.lock().unwrap();
        (
            objects.len() as u64,
            objects.values().map(|value| value.len() as u64).sum(),
        )
    };
    let receipt = rudder_engine::remote_source::ReplicatedRootReceipt {
        format_version: 1,
        root_id: selection.root.root_id.clone(),
        root,
        durability: rudder_engine::remote_source::ReplicatedDurability::Replicated,
        published_at_ms: 1,
        object_count,
        total_bytes,
    };
    (graph, selection, receipt)
}
#[cfg(target_os = "linux")]
struct HttpFixture {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    auth_ok: Arc<AtomicBool>,
    first_bad_header: Arc<Mutex<Option<String>>>,
    complete_requests: Arc<AtomicUsize>,
    incomplete_connections: Arc<AtomicUsize>,
    rejected_routes: Arc<AtomicUsize>,
}
#[cfg(target_os = "linux")]
impl HttpFixture {
    fn new(
        graph: Arc<Graph>,
        receipt: rudder_engine::remote_source::ReplicatedRootReceipt,
    ) -> Self {
        use std::{io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let auth_ok = Arc::new(AtomicBool::new(true));
        let auth = auth_ok.clone();
        let first_bad_header = Arc::new(Mutex::new(None));
        let bad_header = first_bad_header.clone();
        let complete_requests = Arc::new(AtomicUsize::new(0));
        let complete = complete_requests.clone();
        let incomplete_connections = Arc::new(AtomicUsize::new(0));
        let incomplete = incomplete_connections.clone();
        let rejected_routes = Arc::new(AtomicUsize::new(0));
        let rejected = rejected_routes.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_millis(20)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut header = Vec::new();
                        let mut part = [0; 1024];
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while header.len() < 16384
                            && !header.windows(4).any(|bytes| bytes == b"\r\n\r\n")
                            && !stopped.load(Ordering::Acquire)
                            && Instant::now() < deadline
                        {
                            match stream.read(&mut part) {
                                Ok(0) => break,
                                Ok(count) => header.extend_from_slice(&part[..count]),
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) => {}
                                Err(_) => break,
                            }
                        }
                        // A cancelled/preconnected socket is not an HTTP
                        // request. Never return data or classify credentials
                        // until the complete bounded header block arrived.
                        if !header.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                            incomplete.fetch_add(1, Ordering::AcqRel);
                            continue;
                        }
                        let header = String::from_utf8_lossy(&header);
                        let mut request = header.lines().next().unwrap_or("").split_whitespace();
                        let method = request.next().unwrap_or("");
                        let path = request.next().unwrap_or("");
                        let bytes = if method != "GET" {
                            None
                        } else if path == format!("/roots/{}", receipt.root_id) {
                            Some(serde_json::to_vec(&receipt).unwrap())
                        } else {
                            graph
                                .0
                                .lock()
                                .unwrap()
                                .iter()
                                .find_map(|(reference, bytes)| {
                                    let kind = serde_json::to_value(reference.kind).unwrap();
                                    (path
                                        == format!(
                                            "/objects/{}/{}/{}",
                                            kind.as_str().unwrap(),
                                            reference.digest,
                                            reference.size_bytes
                                        ))
                                    .then(|| bytes.clone())
                                })
                        };
                        let Some(bytes) = bytes else {
                            rejected.fetch_add(1, Ordering::AcqRel);
                            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            continue;
                        };
                        complete.fetch_add(1, Ordering::AcqRel);
                        if !header
                            .to_ascii_lowercase()
                            .contains("authorization: bearer native-factory-public-fixture\r\n")
                        {
                            auth.store(false, Ordering::Release);
                            bad_header
                                .lock()
                                .unwrap()
                                .get_or_insert_with(|| header.to_string());
                            let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            continue;
                        }
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            bytes.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(&bytes);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
            auth_ok,
            first_bad_header,
            complete_requests,
            incomplete_connections,
            rejected_routes,
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
#[cfg(target_os = "linux")]
fn runtime_reference(path: PathBuf) -> RuntimeFile {
    let mut file = File::open(&path).unwrap();
    let size_bytes = file.metadata().unwrap().len();
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    RuntimeFile {
        path,
        sha256: encoded_hex(hash.finalize()),
        size_bytes,
    }
}
#[cfg(target_os = "linux")]
fn fixture_runtime(root: &Path) -> (RuntimeFile, RuntimeFile) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let output = std::process::Command::new("node")
        .args(["-p", "process.execPath"])
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("Node22 must be installed");
    assert!(output.status.success());
    let node = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
        .canonicalize()
        .unwrap();
    let module = root.join("fabric-fetch.mjs");
    let output = std::process::Command::new(repository.join("node_modules/.bin/esbuild"))
        .current_dir(repository)
        .arg("src/fabric-fetch.ts")
        .args(["--bundle", "--platform=node", "--format=esm"])
        .arg(format!("--outfile={}", module.display()))
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("npm ci must precede source integration test");
    assert!(output.status.success(), "fixture source bundle failed");
    let script = root.join("helper.mjs");
    fs::write(
        &script,
        format!(
            "import {{runFabricFetchHelper}} from {}; await runFabricFetchHelper();",
            serde_json::to_string(&format!("file://{}", module.display())).unwrap()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o400)).unwrap();
    (runtime_reference(node), runtime_reference(script))
}
fn context_spec(root: &Path) -> ExecutionSpec {
    ExecutionSpec {
        format_version: 4,
        execution_id: uuid(),
        operation_id: uuid(),
        view_id: uuid(),
        owner: OwnerEpoch {
            owner: "owner".into(),
            epoch: 1,
        },
        execution_root: root.join("executions"),
        manager_root: root.join("manager"),
        store_root: root.join("delta"),
        authority_roots: vec![root.into()],
        command: vec!["/usr/bin/true".into()],
        environment: BTreeMap::new(),
        runtime: vec![],
        home_files: vec![],
        broker_directory: None,
        broker_binding: None,
        workspace_observation: None,
        profile: None,
        timeout_seconds: 1,
    }
}
#[test]
#[cfg(target_os = "linux")]
fn actual_source_factory_registers_real_session_and_broker_loss_invalidates_it_without_profile_hydration(
) {
    actual_source_factory(0);
}
#[test]
#[cfg(target_os = "linux")]
fn actual_host_source_survives_guest_denial_and_fences_real_session_on_host_revocation() {
    actual_source_factory(1);
}
#[test]
#[cfg(target_os = "linux")]
fn actual_host_client_expiry_within_outer_window_fences_real_session_stickily() {
    actual_source_factory(2);
}
#[cfg(target_os = "linux")]
struct TestHostPeer;
#[cfg(target_os = "linux")]
impl crate::source_authorizer::HostSourceIssuerPeer for TestHostPeer {
    fn check_current(&self) -> Result<()> {
        Ok(())
    }
    fn check_peer(&self, stream: &std::os::unix::net::UnixStream) -> Result<()> {
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut size,
                )
            } == 0
                && peer.pid == std::process::id() as i32
                && peer.uid == unsafe { libc::geteuid() },
            "fixture peer differs"
        );
        Ok(())
    }
}
#[cfg(target_os = "linux")]
struct TestHostIssuer {
    deny: AtomicBool,
    expires: bool,
}
#[cfg(target_os = "linux")]
impl crate::source_authorizer::HostSourceAuthority for TestHostIssuer {
    fn authorize(
        &self,
        binding: &PartialBinding,
    ) -> Result<rudder_engine::journal::HostSourceAuthorization> {
        ensure!(!self.deny.load(Ordering::Acquire), "fixture host revoked");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;
        Ok(rudder_engine::journal::HostSourceAuthorization {
            identity: rudder_engine::journal::CapabilityIdentity {
                project_id: binding.attempt.project_id.clone(),
                principal: "host-fixture".into(),
                run_id: binding.attempt.run_id.clone(),
                run_epoch: binding.attempt.run_epoch,
                owner_epoch: binding.attempt.owner.epoch,
            },
            validated_at_ms: now,
            expires_at_ms: self.expires.then_some(now + 500),
        })
    }
}
// Model the production source monitor while the factory hashes pinned runtime
// files and opens the real source. Cached grants are never extended locally.
#[cfg(target_os = "linux")]
struct HostRefresh {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
#[cfg(target_os = "linux")]
impl HostRefresh {
    fn start(authority: Arc<PartialAuthority>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                if authority.validate(&authority.state.binding).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for HostRefresh {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
#[cfg(target_os = "linux")]
fn actual_source_factory(mode: u8) {
    use rudder_engine::vfs::{NodeKind, PartialCaptureBudget, PartialCaptureRequest};
    let base = temporary_in(if Path::new("/workspace").is_dir() {
        Path::new("/workspace")
    } else {
        Path::new("/tmp")
    })
    .unwrap();
    let (node, script) = fixture_runtime(base.path());
    let (graph, selection, receipt) = graph();
    let http = HttpFixture::new(graph, receipt);
    // Model a transport cancellation before a request was written. The
    // fixture must still check authorization on all subsequent real requests.
    drop(std::net::TcpStream::connect(http.url.strip_prefix("http://").unwrap()).unwrap());
    // Workspace port health checks can arrive independently of this fixture.
    // Unknown routes/methods must return no source data and cannot count as
    // authenticated fabric reads.
    {
        use std::io::Write;
        let mut probe =
            std::net::TcpStream::connect(http.url.strip_prefix("http://").unwrap()).unwrap();
        probe
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        probe
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        probe
            .write_all(b"HEAD / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        probe.read_to_end(&mut response).unwrap();
        assert_eq!(
            response,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }
    let tracker = Arc::new(SessionTracker(Mutex::new(None)));
    let clock = Arc::new(AtomicU64::new(1));
    let now = clock.clone();
    let mut state = AuthorityState::new(
        expected_binding(&selection).unwrap(),
        Arc::new(move || Ok(now.load(Ordering::Acquire) as u128)),
        &tracker,
    );
    let broker = Arc::new(Broker {
        calls: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
        advance: Mutex::new(None),
    });
    let host_issuer = Arc::new(TestHostIssuer {
        deny: AtomicBool::new(false),
        expires: mode == 2,
    });
    let mut host_server = (mode != 0).then(|| {
        crate::source_authorizer::SourceAuthorizerServer::bind(
            &base.path().join("host"),
            state.binding.clone(),
            host_issuer.clone(),
            crate::source_authorizer::Limits::default(),
        )
        .unwrap()
    });
    let host_client = host_server.as_ref().map(|server| {
        Arc::new(
            crate::source_authorizer::SourceAuthorizerClient::connect(
                server.directory(),
                state.binding.clone(),
                Arc::new(TestHostPeer),
            )
            .unwrap(),
        )
    });
    if let Some(host) = &host_client {
        Arc::get_mut(&mut state).unwrap().host = Some(host.clone());
    }
    let ingress = SeparatedIngress {
        broker: broker.clone(),
        source: state.clone(),
        fenced: AtomicBool::new(false),
    };
    let source_broker: Arc<dyn BrokerAuthorization> = match &host_client {
        Some(host) => host.clone(),
        None => broker.clone(),
    };
    let authority = Arc::new(PartialAuthority {
        broker: source_broker,
        state,
    });
    authority.validate(&authority.state.binding).unwrap();
    let refresh = (mode != 0).then(|| HostRefresh::start(authority.clone()));
    let context = HostContext {
        format_version: 1,
        kind: KIND.into(),
        launch_request_digest: hex_digest(canonical_json(&selection).unwrap()),
        node,
        cli_script: script,
        source: SourceContext {
            scope: selection.root.scope.clone(),
            base_url: http.url.clone(),
            auth_headers: BTreeMap::from([(
                "Authorization".into(),
                "Bearer native-factory-public-fixture".into(),
            )]),
        },
        cache_root: base.path().join("cache"),
        registry_root: base.path().join("registry"),
    };
    let spec = context_spec(base.path());
    assert!(validate_context(&context, &spec, &selection, TransportProfile::Https).is_err());
    validate_context(
        &context,
        &spec,
        &selection,
        TransportProfile::LoopbackHttpTest,
    )
    .unwrap();
    let node = PinnedRuntime::open(&context.node, true, RuntimeTrust::Fixture).unwrap();
    let script = PinnedRuntime::open(&context.cli_script, false, RuntimeTrust::Fixture).unwrap();
    let (workspace, resources) = open_workspace(
        context,
        &selection,
        &spec.manager_root,
        &spec.store_root,
        authority.clone(),
        tracker.clone(),
        node,
        script,
        TransportProfile::LoopbackHttpTest,
    )
    .unwrap();
    assert_eq!(
        workspace
            .view()
            .descriptor()
            .unwrap()
            .remote_finalization_format,
        Some(1)
    );
    assert!(!workspace
        .manager()
        .store()
        .object_path(&selection.observation.profile)
        .unwrap()
        .exists());
    let session = tracker.0.lock().unwrap().as_ref().unwrap().clone();
    session.check_current().unwrap();
    let file = workspace
        .view()
        .create(1, b"source", NodeKind::File, 0o600, b"")
        .unwrap();
    workspace
        .view()
        .write(file.ino, 0, b"retained fixture", false)
        .unwrap();
    let capture = workspace
        .view()
        .capture_partial_source(
            &ImportPolicy::default(),
            &PartialCaptureRequest {
                nonce: uuid(),
                expected_begin: workspace.view().dirty_cursor().unwrap(),
                expected_head: None,
            },
            &PartialCaptureBudget::default(),
        )
        .unwrap();
    let retained = workspace.manager().store().retention().unwrap().pins;
    drop(refresh);
    if mode == 1 {
        ingress.validate(&authority.state.binding).unwrap();
        broker.deny.store(true, Ordering::Release);
        assert!(ingress.validate(&authority.state.binding).is_err());
        session.check_current().unwrap();
        authority.validate(&authority.state.binding).unwrap();
        broker.deny.store(false, Ordering::Release);
        assert!(ingress.validate(&authority.state.binding).is_err());
        session.check_current().unwrap();
        host_issuer.deny.store(true, Ordering::Release);
        assert!(authority.validate(&authority.state.binding).is_err());
    } else if mode == 2 {
        std::thread::sleep(Duration::from_millis(510));
        // Outer mirror has not expired: only actual host-client expiry can fence.
        assert!(authority.state.freshness.lock().unwrap().until.unwrap() > 1);
        assert!(session.check_current().is_err());
    } else {
        broker.deny.store(true, Ordering::Release);
        assert!(authority.validate(&authority.state.binding).is_err());
    }
    assert!(session.check_current().is_err());
    if let Some(host) = host_client {
        assert!(host.check_current().is_err());
        assert!(host.authorize(&authority.state.binding).is_err());
        assert!(ingress.cancelled());
    }
    assert!(workspace.view().dirty_cursor().is_err());
    assert_eq!(
        workspace.manager().store().retention().unwrap().pins,
        retained
    );
    assert!(capture.retention.local.object_count > 0);
    assert!(http.complete_requests.load(Ordering::Acquire) > 0);
    assert!(http.incomplete_connections.load(Ordering::Acquire) > 0);
    assert!(http.rejected_routes.load(Ordering::Acquire) > 0);
    assert!(
        http.auth_ok.load(Ordering::Acquire),
        "public fixture request: {:?}",
        http.first_bad_header.lock().unwrap()
    );
    let source = resources.source.clone();
    drop(resources);
    let end = Instant::now() + Duration::from_secs(5);
    while !source.stats().unwrap().child_exited && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(source.stats().unwrap().child_exited);
    if let Some(server) = &mut host_server {
        server.await_closed(Duration::from_secs(2)).unwrap();
    }
    // This establishes owned-child exit only, not a whole transport/thread drain.
}

#[test]
fn protected_source_context_rejects_unknown_duplicate_wrong_scope_http_and_unprotected_roots() {
    let base = temporary().unwrap();
    let (_, selection, _) = graph();
    let spec = context_spec(base.path());
    let value = serde_json::json!({"format_version":1,"kind":KIND,"launch_request_digest":hex_digest(canonical_json(&selection).unwrap()),
        "node":{"path":"/usr/local/bin/node","sha256":"a".repeat(64),"size_bytes":1},"cli_script":{"path":"/opt/rudder/dist/index.js","sha256":"b".repeat(64),"size_bytes":1},
        "source":{"scope":selection.root.scope,"base_url":"https://source.invalid","auth_headers":{"Authorization":"Bearer private-context-fixture"}},"cache_root":base.path().join("cache"),"registry_root":base.path().join("registry")});
    let file = base.path().join("context.json");
    let write = |bytes: Vec<u8>| {
        fs::write(&file, &bytes).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        ProtectedHostContextRef {
            format_version: 1,
            path: file.clone(),
            sha256: hex_digest(&bytes),
            size_bytes: bytes.len() as u64,
        }
    };
    let reference = write(canonical_json(&value).unwrap());
    read_context(&reference, &spec, &selection).unwrap();
    let mut spec5 = context_spec(base.path());
    spec5.format_version = 5;
    assert!(read_host_context(&reference, &spec5, &selection).is_err());
    assert!(read_context(&reference, &spec5, &selection).is_err());
    let directory = base.path().join("host-authorizer");
    private_directory(&directory).unwrap();
    let mut v2 = value.clone();
    v2["format_version"] = serde_json::json!(2);
    v2["source_authorizer_directory"] = serde_json::json!(directory);
    let reference2 = write(canonical_json(&v2).unwrap());
    read_host_context(&reference2, &spec5, &selection).unwrap();
    assert!(read_context(&reference2, &spec, &selection).is_err());
    assert!(read_host_context(&reference2, &spec, &selection).is_err());
    for change in 0..8 {
        let mut bad = v2.clone();
        match change {
            0 => bad["extra"] = serde_json::json!(true),
            1 => bad["format_version"] = serde_json::json!(1),
            2 => {
                bad.as_object_mut()
                    .unwrap()
                    .remove("source_authorizer_directory");
            }
            3 => bad["source_authorizer_directory"] = serde_json::json!(base.path().join("absent")),
            4 => bad["source_authorizer_directory"] = bad["cache_root"].clone(),
            5 => bad["source_authorizer_directory"] = serde_json::json!("/unprotected/host"),
            6 => bad["source"]["base_url"] = serde_json::json!("http://127.0.0.1:1234"),
            7 => bad["source"]["scope"]["tenant_id"] = serde_json::json!("other"),
            _ => unreachable!(),
        }
        assert!(
            read_host_context(&write(canonical_json(&bad).unwrap()), &spec5, &selection).is_err()
        );
    }
    for mode in [0o755, 0o1700] {
        fs::set_permissions(&directory, fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            read_host_context(&write(canonical_json(&v2).unwrap()), &spec5, &selection).is_err()
        );
    }
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let linked = base.path().join("host-link");
    std::os::unix::fs::symlink(&directory, &linked).unwrap();
    let mut linked_context = v2.clone();
    linked_context["source_authorizer_directory"] = serde_json::json!(linked);
    assert!(read_host_context(
        &write(canonical_json(&linked_context).unwrap()),
        &spec5,
        &selection
    )
    .is_err());
    let canonical2 = String::from_utf8(canonical_json(&v2).unwrap()).unwrap();
    let duplicate2 = format!("{{\"format_version\":2,{}", &canonical2[1..]);
    assert!(read_host_context(&write(duplicate2.into_bytes()), &spec5, &selection).is_err());
    for change in 0..5 {
        let mut bad = value.clone();
        match change {
            0 => bad["extra"] = serde_json::json!(true),
            1 => bad["source"]["scope"]["tenant_id"] = serde_json::json!("other"),
            2 => bad["source"]["base_url"] = serde_json::json!("http://127.0.0.1:1234"),
            3 => bad["cache_root"] = serde_json::json!("/unprotected/cache"),
            4 => {
                bad["source"]["auth_headers"]["Cookie"] =
                    serde_json::json!("private-context-fixture")
            }
            _ => unreachable!(),
        }
        let reference = write(canonical_json(&bad).unwrap());
        let error = read_context(&reference, &spec, &selection)
            .err()
            .expect("bad private context accepted");
        assert!(!error.to_string().contains("private-context-fixture"));
    }
    let canonical = String::from_utf8(canonical_json(&value).unwrap()).unwrap();
    let duplicate = format!("{{\"format_version\":1,{}", &canonical[1..]);
    let reference = write(duplicate.into_bytes());
    assert!(read_context(&reference, &spec, &selection).is_err());
    let mut reference = write(canonical_json(&value).unwrap());
    reference.sha256 = "f".repeat(64);
    assert!(read_context(&reference, &spec, &selection).is_err());
}

// A one millisecond cached capability must not inherit the legacy three second
// mirror. This clock is deterministic; actual-client/session coverage is above.
#[test]
fn host_cached_checks_gate_each_root_and_expiry_is_absorbing() {
    struct ShortHost {
        time: Arc<AtomicU64>,
        retired: AtomicBool,
    }
    impl BrokerAuthorization for ShortHost {
        fn authorize(&self, _: &PartialBinding) -> Result<()> {
            self.check_current()
        }
        fn check_current(&self) -> Result<()> {
            ensure!(
                !self.retired.load(Ordering::Acquire)
                    && self.time.load(Ordering::Acquire) < 1_000_001,
                "short host expired"
            );
            Ok(())
        }
        fn invalidate(&self) {
            self.retired.store(true, Ordering::Release);
        }
    }
    let (legacy, _clock, tracker, _) = fixture_authority();
    let time = Arc::new(AtomicU64::new(1));
    let host = Arc::new(ShortHost {
        time: time.clone(),
        retired: AtomicBool::new(false),
    });
    let mut state = AuthorityState::new(legacy.state.binding.clone(), Arc::new(|| Ok(1)), &tracker);
    Arc::get_mut(&mut state).unwrap().host = Some(host.clone());
    let authority = PartialAuthority {
        broker: host.clone(),
        state: state.clone(),
    };
    authority.validate(&state.binding).unwrap();
    let root = RootAuthority(state.clone());
    root.validate(&state.binding.attempt, &state.binding.root)
        .unwrap();
    time.store(1_000_001, Ordering::Release);
    assert!(root
        .validate(&state.binding.attempt, &state.binding.root)
        .is_err());
    assert!(host.retired.load(Ordering::Acquire));
    time.store(1, Ordering::Release);
    assert!(authority.validate(&state.binding).is_err());
}
