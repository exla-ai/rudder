#![cfg(unix)]
//! Real Unix framing/auth/capture/settlement with an actual strict remote
//! resolver. No mounts, host namespaces, provider credentials or network calls.
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use rudder_engine::{
    directory::{DirectoryStore, PagedDirectory},
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::{CacheLimits, CacheScope, FetchControl, ImmutableObjectSource, RemoteCache},
    remote_root::{
        AttemptBinding, CurrentAttemptAuthority, RemoteRootRegistry, RootBinding, RootLimits,
        RootReceiptSource,
    },
    remote_source::{
        ReplicatedDurability, ReplicatedRootReceipt, SourceError, SourceRequestControl,
    },
    snapshot::{RunnableRoot, SourceV2},
    store::{ImportPolicy, LocalStore},
    vfs::{
        exclusion_policy_digest,
        partial_rpc::*,
        remote_launch::{self, RemoteLaunchRequest},
        rpc::{self, AttemptContext, ObservationBinding, RpcError},
        DirtyCursor, DirtyPage, NodeKind, ObservationConsistency, PartialCaptureReceipt,
        PartialCaptureRequest, PartialCaptureStatus, RemoteOptions, VfsManager, VfsView,
    },
    workspace::WorkspaceProfile,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

fn nonce() -> String {
    Uuid::new_v4().to_string()
}
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "bounded fixture did not settle");
        thread::sleep(Duration::from_millis(2));
    }
}
fn private(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

struct RootAuthority(AtomicBool);
impl CurrentAttemptAuthority for RootAuthority {
    fn validate(&self, _: &AttemptBinding, _: &RootBinding) -> Result<()> {
        ensure!(
            self.0.load(Ordering::Acquire),
            "fixture remote root revoked"
        );
        Ok(())
    }
}
struct Authority {
    current: AtomicBool,
    calls: AtomicUsize,
    block_on: AtomicUsize,
    entered: AtomicBool,
    gate: (Mutex<bool>, Condvar),
    expected: Mutex<Option<PartialBinding>>,
}
impl Authority {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            current: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            block_on: AtomicUsize::new(usize::MAX),
            entered: AtomicBool::new(false),
            gate: (Mutex::new(false), Condvar::new()),
            expected: Mutex::new(None),
        })
    }
    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}
impl PartialAttemptAuthority for Authority {
    fn validate(&self, binding: &PartialBinding) -> Result<()> {
        ensure!(
            self.current.load(Ordering::Acquire),
            "fixture current grant rejected"
        );
        if let Some(expected) = self.expected.lock().unwrap().as_ref() {
            ensure!(binding == expected, "fixture exact partial scope differs");
        }
        let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call == self.block_on.load(Ordering::Acquire) {
            self.entered.store(true, Ordering::Release);
            // Deliberately model a misbehaving trusted callback to establish
            // that timeout/cancel cannot stand in for actual server settlement.
            let guard = self.gate.0.lock().unwrap();
            let (guard, wait) = self
                .gate
                .1
                .wait_timeout_while(guard, Duration::from_secs(3), |released| !*released)
                .unwrap();
            ensure!(*guard && !wait.timed_out(), "fixture callback fuse expired");
        }
        ensure!(
            self.current.load(Ordering::Acquire),
            "fixture authority lost while settling"
        );
        Ok(())
    }
    fn cancelled(&self) -> bool {
        !self.current.load(Ordering::Acquire)
    }
}
struct Source {
    scope: CacheScope,
    objects: Mutex<BTreeMap<ObjectRef, Vec<u8>>>,
    receipt: Mutex<Option<ReplicatedRootReceipt>>,
    calls: AtomicUsize,
}
impl Source {
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> ObjectRef {
        let reference = ObjectRef::from_bytes(kind, bytes);
        self.objects
            .lock()
            .unwrap()
            .insert(reference.clone(), bytes.into());
        reference
    }
    fn metadata(&self, kind: ObjectKind, value: &impl Serialize) -> ObjectRef {
        self.put(kind, &canonical_json(value).unwrap())
    }
}
impl DirectoryStore for Source {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.objects
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .context("fixture object missing")
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        Ok(Source::put(self, kind, bytes))
    }
}
impl ImmutableObjectSource for Source {
    fn scope(&self) -> CacheScope {
        self.scope.clone()
    }
    fn fetch(
        &self,
        reference: &ObjectRef,
        control: &FetchControl,
        output: &mut dyn Write,
    ) -> Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        control.check()?;
        output.write_all(&DirectoryStore::read(self, reference)?)?;
        Ok(())
    }
}
impl RootReceiptSource for Source {
    fn scope(&self) -> CacheScope {
        self.scope.clone()
    }
    fn get_root(
        &self,
        _: &str,
        _: &SourceRequestControl,
    ) -> std::result::Result<ReplicatedRootReceipt, SourceError> {
        self.receipt
            .lock()
            .unwrap()
            .clone()
            .ok_or(SourceError::RemoteFailed)
    }
}
struct Fixture {
    server: Option<PartialRpc>,
    view: VfsView,
    manager: VfsManager,
    store: LocalStore,
    request: RemoteLaunchRequest,
    source: Arc<Source>,
    root_authority: Arc<RootAuthority>,
    authority: Arc<Authority>,
    _registry: RemoteRootRegistry,
    _cache: RemoteCache,
    endpoint: tempfile::TempDir,
    _data: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let data = if Path::new("/workspace").is_dir() {
            tempfile::tempdir_in("/workspace").unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        private(data.path());
        // Explicit short portable Unix socket path, including on Darwin.
        let endpoint = tempfile::Builder::new()
            .prefix("prpc-")
            .tempdir_in("/tmp")
            .unwrap();
        private(endpoint.path());
        let source = Arc::new(Source {
            scope: CacheScope {
                provider_id: "partial-rpc-fixture".into(),
                tenant_id: nonce(),
            },
            objects: Mutex::new(BTreeMap::new()),
            receipt: Mutex::new(None),
            calls: AtomicUsize::new(0),
        });
        let directory = PagedDirectory::new(source.as_ref())
            .build(0o755, [])
            .unwrap();
        let seed = PagedDirectory::new(source.as_ref())
            .build(0o700, [])
            .unwrap();
        let profile = source.metadata(ObjectKind::Blob, &WorkspaceProfile::source_only());
        let policy = source.put(ObjectKind::Blob, br#"{"format_version":1}"#);
        let accepted_source = source.metadata(
            ObjectKind::SourceRoot,
            &SourceV2 {
                format_version: 2,
                consistency: CheckpointConsistency::Quiesced,
                directory: directory.clone(),
                projection_policy: policy.clone(),
            },
        );
        let hardlinks = source.metadata(ObjectKind::Blob, &json!({"format_version":1,"groups":[]}));
        let root = source.metadata(
            ObjectKind::RunnableRoot,
            &RunnableRoot {
                format_version: 1,
                consistency: CheckpointConsistency::Quiesced,
                source: accepted_source.clone(),
                environment: None,
                view_root: directory,
                profile: profile.clone(),
                hardlinks,
                knowledge_cursor: 0,
            },
        );
        let (object_count, total_bytes) = {
            let objects = source.objects.lock().unwrap();
            (
                objects.len() as u64,
                objects.values().map(|value| value.len() as u64).sum(),
            )
        };
        *source.receipt.lock().unwrap() = Some(ReplicatedRootReceipt {
            format_version: 1,
            root_id: "fixture-root".into(),
            root: root.clone(),
            durability: ReplicatedDurability::Replicated,
            published_at_ms: 1,
            object_count,
            total_bytes,
        });
        for name in ["cache", "registry"] {
            private(&data.path().join(name));
        }
        let cache = RemoteCache::open(
            data.path().join("cache"),
            source.clone(),
            CacheLimits::default(),
        )
        .unwrap();
        let root_authority = Arc::new(RootAuthority(AtomicBool::new(true)));
        let registry = RemoteRootRegistry::open(
            data.path().join("registry"),
            cache.clone(),
            source.clone(),
            root_authority.clone(),
            RootLimits::default(),
        )
        .unwrap();
        let attempt = AttemptBinding {
            project_id: "project".into(),
            run_id: "run".into(),
            run_epoch: 1,
            owner: OwnerEpoch {
                owner: "owner".into(),
                epoch: 1,
            },
            execution_id: nonce(),
            operation_id: nonce(),
            view_id: nonce(),
        };
        let request = RemoteLaunchRequest {
            format_version: 1,
            request_id: nonce(),
            root: RootBinding {
                scope: source.scope.clone(),
                root_id: "fixture-root".into(),
                root,
            },
            observation: ObservationBinding {
                format_version: 1,
                attempt: AttemptContext {
                    execution_id: attempt.execution_id.clone(),
                    operation_id: attempt.operation_id.clone(),
                },
                view_id: attempt.view_id.clone(),
                owner: attempt.owner.clone(),
                namespace: nonce(),
                profile,
                exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
            },
            attempt,
            projection_policy: policy,
            accepted_source,
            accepted_seed: seed,
        };
        let session = registry
            .admit(
                &request.attempt,
                &request.root.root_id,
                &request.root.root,
                &SourceRequestControl::new(Instant::now() + Duration::from_secs(2)),
            )
            .unwrap();
        let selected =
            remote_launch::validate_selection(session, &request, RemoteOptions::default()).unwrap();
        let store = LocalStore::open(data.path().join("delta")).unwrap();
        let workspace = selected
            .open_finalizable(data.path().join("view"), store.clone())
            .unwrap();
        let (manager, view, actual) = workspace.into_parts();
        assert_eq!(actual, request);
        Self {
            server: None,
            view,
            manager,
            store,
            request,
            source,
            root_authority,
            authority: Authority::new(),
            _registry: registry,
            _cache: cache,
            endpoint,
            _data: data,
        }
    }
    fn config(&self) -> PartialRpcConfig {
        PartialRpcConfig {
            authority: self.authority.clone(),
            limits: PartialRpcLimits::default(),
        }
    }
    fn start(&mut self) {
        self.start_with(PartialRpcLimits::default());
    }
    fn start_with(&mut self, limits: PartialRpcLimits) {
        let server = PartialRpc::bind(
            self.view.clone(),
            PartialRpcConfig {
                authority: self.authority.clone(),
                limits,
            },
            &self.endpoint.path().join("endpoint"),
        )
        .unwrap();
        *self.authority.expected.lock().unwrap() = Some(server.binding().clone());
        self.server = Some(server);
    }
    fn rpc(&self) -> &PartialRpc {
        self.server.as_ref().unwrap()
    }
    fn client(&self) -> PartialClient {
        PartialClient::new(
            self.rpc().socket_path(),
            self.rpc().identity_path(),
            self.rpc().binding().clone(),
        )
        .unwrap()
    }
    fn cursor(&self) -> DirtyCursor {
        let Reply::Cursor(value) = self.client().call(Operation::Cursor).unwrap() else {
            panic!("wrong cursor reply")
        };
        value.decode().unwrap()
    }
    fn write(&self, name: &[u8], bytes: &[u8]) {
        let node = self
            .view
            .create(1, name, NodeKind::File, 0o600, b"")
            .unwrap();
        self.view.write(node.ino, 0, bytes, false).unwrap();
    }
    fn capture(&self, request: &PartialCaptureRequest) -> PartialCaptureReceipt {
        let Reply::CapturePartial(value) = self
            .client()
            .call(Operation::CapturePartial {
                request: OpaqueJson::from_serializable(request).unwrap(),
            })
            .unwrap()
        else {
            panic!("wrong capture reply")
        };
        value.decode().unwrap()
    }
    fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            let settled = server.await_closed(Duration::from_secs(3)).unwrap();
            assert_eq!(settled.binding_digest(), server.binding().digest().unwrap());
        }
    }
    fn packet(&self, operation: Value) -> Value {
        json!({"protocol":PROTOCOL,"format_version":1,"id":nonce(),"identity":read_identity(&self.rpc().identity_path()).unwrap(),"operation":operation})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.authority.release();
        if let Some(server) = self.server.take() {
            let _ = server.await_closed(Duration::from_secs(3));
        }
    }
}
fn send_bytes(stream: &mut UnixStream, bytes: &[u8]) {
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(bytes).unwrap();
}
fn send(stream: &mut UnixStream, value: &Value) {
    send_bytes(stream, &serde_json::to_vec(value).unwrap());
}
fn response(stream: &mut UnixStream) -> Result<Value> {
    stream.set_read_timeout(Some(Duration::from_millis(20)))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exact = |bytes: &mut [u8]| -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(Instant::now() < deadline, "fixture response deadline");
            match stream.read(&mut bytes[offset..]) {
                Ok(0) => bail!("response closed"),
                Ok(count) => offset += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    };
    let mut prefix = [0; 4];
    exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    ensure!(length > 0 && length <= MAX_RESPONSE_BYTES, "response bound");
    let mut bytes = vec![0; length];
    exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn call_raw(f: &Fixture, value: &Value) -> Value {
    let mut stream = UnixStream::connect(f.rpc().socket_path()).unwrap();
    send(&mut stream, value);
    response(&mut stream).unwrap()
}
fn error_code(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<RpcError>()
        .map(|value| value.code.as_str())
}

#[test]
fn actual_strict_binding_is_derived_without_local_profile_hydration_and_families_stay_distinct() {
    let mut f = Fixture::new();
    let before = f.source.calls.load(Ordering::Acquire);
    assert!(!f
        .store
        .object_path(&f.request.observation.profile)
        .unwrap()
        .exists());
    f.start();
    assert_eq!(f.source.calls.load(Ordering::Acquire), before);
    assert_eq!(
        f.rpc().binding().launch_request_digest,
        hex::encode(Sha256::digest(canonical_json(&f.request).unwrap()))
    );
    assert_eq!(f.rpc().binding().root, f.request.root);
    assert_eq!(f.rpc().binding().attempt, f.request.attempt);
    assert_eq!(f.rpc().binding().observation, f.request.observation);
    assert_eq!(f.rpc().socket_path().file_name().unwrap(), SOCKET_NAME);
    assert_eq!(f.rpc().identity_path().file_name().unwrap(), IDENTITY_NAME);
    assert_eq!(REQUIRED_OPERATION, "workspace:partial");
    assert!(partial_scope(f.rpc().binding())
        .unwrap()
        .starts_with("partial-workspace:"));
    assert!(rpc::read_identity(&f.rpc().identity_path()).is_err());
    assert_eq!(f.cursor().namespace, f.request.observation.namespace);
    assert_eq!(f.source.calls.load(Ordering::Acquire), before);
    assert!(!f
        .store
        .object_path(&f.request.observation.profile)
        .unwrap()
        .exists());
    let mut changed = f.rpc().binding().clone();
    changed.launch_request_digest = "f".repeat(64);
    assert_ne!(
        partial_scope(&changed).unwrap(),
        partial_scope(f.rpc().binding()).unwrap()
    );
    assert!(PartialClient::new(f.rpc().socket_path(), f.rpc().identity_path(), changed).is_err());
}

#[test]
fn opaque_documents_preserve_full_u64_and_reject_unknown_duplicate_operations_and_inner_fields() {
    let cursor = DirtyCursor {
        view_id: nonce(),
        owner: OwnerEpoch {
            owner: "fixture".into(),
            epoch: 1,
        },
        namespace: nonce(),
        generation: u64::MAX,
    };
    let document = OpaqueJson::from_serializable(&cursor).unwrap();
    assert!(document.as_json().contains("18446744073709551615"));
    let operation = Operation::Dirty {
        after: document.clone(),
        limit: 1,
    };
    let wire = serde_json::to_vec(&operation).unwrap();
    let parsed: Operation = serde_json::from_slice(&wire).unwrap();
    assert_eq!(parsed, operation);
    assert_eq!(document.decode::<DirtyCursor>().unwrap(), cursor);
    for bad in [
        r#"{"operation":"cursor","extra":1}"#,
        r#"{"operation":"partial_head","witness":true}"#,
        r#"{"operation":"cursor","operation":"cursor"}"#,
        r#"{"operation":"observe","nonce":"x"}"#,
        r#"{"operation":"adopt"}"#,
        r#"{"operation":"release"}"#,
        r#"{"operation":"freeze"}"#,
        r#"{"operation":"checkpoint"}"#,
    ] {
        assert!(
            serde_json::from_str::<Operation>(bad).is_err(),
            "accepted {bad}"
        );
    }
    assert!(OpaqueJson::from_json(br#"{"generation":1,"generation":2}"#).is_err());
    assert!(OpaqueJson::from_json(br#"{"nested":{"x":1,"x":2}}"#).is_err());
    assert!(OpaqueJson::from_json(&vec![b' '; MAX_RESPONSE_BYTES + 1]).is_err());
    assert!(OpaqueJson::from_serializable(&"x".repeat(MAX_RESPONSE_BYTES)).is_err());
    let bad = serde_json::to_vec(
        &json!({"operation":"dirty","after":"{\"generation\":1,\"generation\":2}","limit":1}),
    )
    .unwrap();
    assert!(serde_json::from_slice::<Operation>(&bad).is_err());
}

#[test]
fn fifo_identity_is_rejected_before_any_wait_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let endpoint = tempfile::tempdir().unwrap();
    let fifo = endpoint.path().join("identity-fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (send, receive) = std::sync::mpsc::channel();
    let reader_path = fifo.clone();
    let reader = thread::spawn(move || {
        let _ = send.send(read_identity(&reader_path).is_err());
    });
    let prompt = receive.recv_timeout(Duration::from_secs(1));
    if prompt.is_err() {
        // Unblock the old blocking-open defect before reporting regression.
        // Never hang the test harness on a deliberately hostile FIFO.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if let Ok(writer) = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                drop(writer);
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let _ = receive.recv_timeout(Duration::from_secs(1));
    }
    if reader.is_finished() {
        reader.join().unwrap();
    }
    assert_eq!(
        prompt.ok(),
        Some(true),
        "identity FIFO open waited for a writer"
    );
}

#[test]
fn capture_cursor_dirty_inspect_and_head_are_data_only_durable_idempotent_retries() {
    let mut f = Fixture::new();
    f.start();
    let before = f.cursor();
    f.write(b"source", b"first");
    let Reply::Dirty(dirty) = f
        .client()
        .call(Operation::Dirty {
            after: OpaqueJson::from_serializable(&before).unwrap(),
            limit: 128,
        })
        .unwrap()
    else {
        panic!("not dirty")
    };
    let dirty: DirtyPage = dirty.decode().unwrap();
    assert!(!dirty.events.is_empty());
    let request = PartialCaptureRequest {
        nonce: nonce(),
        expected_begin: f.cursor(),
        expected_head: None,
    };
    let receipt = f.capture(&request);
    assert_eq!(receipt.consistency, ObservationConsistency::LiveBestEffort);
    assert_eq!(receipt.attempt, f.request.attempt);
    assert_eq!(receipt.base, f.request.root);
    let Reply::InspectPartial(status) = f
        .client()
        .call(Operation::InspectPartial {
            nonce: request.nonce.clone(),
        })
        .unwrap()
    else {
        panic!("not inspect")
    };
    let status: Option<PartialCaptureStatus> = status.decode().unwrap();
    assert_eq!(status.as_ref().unwrap().receipt.as_ref(), Some(&receipt));
    assert!(status.unwrap().current);
    let Reply::PartialHead(head) = f.client().call(Operation::PartialHead).unwrap() else {
        panic!("not head")
    };
    assert_eq!(
        head.decode::<Option<PartialCaptureReceipt>>().unwrap(),
        Some(receipt.clone())
    );
    f.write(b"later", b"after original capture");
    assert_eq!(f.capture(&request), receipt);
    let retention = f.store.retention().unwrap();
    let mut changed = request.clone();
    changed.expected_begin = f.cursor();
    assert!(f
        .client()
        .call(Operation::CapturePartial {
            request: OpaqueJson::from_serializable(&changed).unwrap()
        })
        .is_err());
    assert_eq!(f.store.retention().unwrap().pins, retention.pins);
    let old_identity = read_identity(&f.rpc().identity_path()).unwrap();
    f.stop();
    f.start();
    assert_ne!(
        read_identity(&f.rpc().identity_path()).unwrap().token,
        old_identity.token
    );
    let mut old = f.packet(json!({"operation":"cursor"}));
    old["identity"] = serde_json::to_value(old_identity).unwrap();
    assert_eq!(call_raw(&f, &old)["error"]["code"], "unauthenticated");
    let generation = f.cursor().generation;
    assert_eq!(f.capture(&request), receipt);
    assert_eq!(f.cursor().generation, generation);
    assert_eq!(f.store.retention().unwrap().pins, retention.pins);
}

#[test]
fn forged_identity_foreign_exact_scope_and_both_current_authorities_deny_before_capture() {
    let mut f = Fixture::new();
    f.start();
    let before = f.store.retention().unwrap().pins;
    for mutation in ["token", "namespace", "request", "run", "root"] {
        let mut packet = f.packet(json!({"operation":"cursor"}));
        match mutation {
            "token" => packet["identity"]["token"] = json!("0".repeat(64)),
            "namespace" => {
                packet["identity"]["binding"]["observation"]["namespace"] = json!(nonce())
            }
            "request" => {
                packet["identity"]["binding"]["launch_request_digest"] = json!("f".repeat(64))
            }
            "run" => packet["identity"]["binding"]["attempt"]["run_id"] = json!("foreign"),
            "root" => packet["identity"]["binding"]["root"]["root_id"] = json!("foreign"),
            _ => unreachable!(),
        }
        assert_eq!(
            call_raw(&f, &packet)["error"]["code"],
            "unauthenticated",
            "{mutation}"
        );
    }
    let mut wrong = f.rpc().binding().clone();
    wrong.launch_request_digest = "f".repeat(64);
    *f.authority.expected.lock().unwrap() = Some(wrong);
    assert_eq!(
        error_code(&f.client().call(Operation::Cursor).unwrap_err()),
        Some("stale_attempt")
    );
    *f.authority.expected.lock().unwrap() = Some(f.rpc().binding().clone());
    f.authority.current.store(false, Ordering::Release);
    assert_eq!(
        error_code(&f.client().call(Operation::Cursor).unwrap_err()),
        Some("stale_attempt")
    );
    f.authority.current.store(true, Ordering::Release);
    f.root_authority.0.store(false, Ordering::Release);
    assert!(f.client().call(Operation::Cursor).is_err());
    assert_eq!(f.store.retention().unwrap().pins, before);
}

#[test]
fn raw_frame_unknown_duplicate_nested_and_complete_protocol_bytes_are_refused_without_effects() {
    let mut f = Fixture::new();
    f.start();
    let before = f.store.retention().unwrap().pins;
    let valid = serde_json::to_string(&f.packet(json!({"operation":"cursor"}))).unwrap();
    let mut bad = vec![
        format!("{{\"format_version\":1,{}", &valid[1..]),
        valid.replacen("\"run_epoch\":1", "\"run_epoch\":1,\"run_epoch\":1", 1),
    ];
    for operation in [
        json!({"operation":"cursor","extra":true}),
        json!({"operation":"observe","nonce":nonce()}),
        json!({"operation":"adopt"}),
        json!({"operation":"release"}),
        json!({"operation":"freeze"}),
        json!({"operation":"checkpoint"}),
    ] {
        bad.push(serde_json::to_string(&f.packet(operation)).unwrap());
    }
    let mut wrong = f.packet(json!({"operation":"cursor"}));
    wrong["protocol"] = json!("complete-workspace-v1");
    bad.push(serde_json::to_string(&wrong).unwrap());
    let mut complete = f.packet(json!({"operation":"cursor"}));
    complete.as_object_mut().unwrap().remove("protocol");
    bad.push(serde_json::to_string(&complete).unwrap());
    for bytes in bad {
        let mut stream = UnixStream::connect(f.rpc().socket_path()).unwrap();
        send_bytes(&mut stream, bytes.as_bytes());
        assert!(response(&mut stream).is_err());
    }
    for length in [0, MAX_REQUEST_BYTES as u32 + 1] {
        let mut stream = UnixStream::connect(f.rpc().socket_path()).unwrap();
        stream.write_all(&length.to_be_bytes()).unwrap();
        assert!(response(&mut stream).is_err());
    }
    let request = json!({"nonce":nonce(),"expected_begin":f.cursor(),"expected_head":null,"policy":{"exclude":[]},"adoption_witness":true});
    let reply=call_raw(&f,&f.packet(json!({"operation":"capture_partial","request":serde_json::to_string(&request).unwrap()})));
    assert_eq!(reply["error"]["code"], "operation_failed");
    until(|| f.rpc().activity().connections == 0);
    assert_eq!(f.store.retention().unwrap().pins, before);
}

#[test]
fn actual_view_lock_prevents_second_ingress_and_only_settled_close_releases_it() {
    let mut f = Fixture::new();
    f.start();
    assert!(PartialRpc::bind(
        f.view.clone(),
        f.config(),
        &f.endpoint.path().join("second")
    )
    .is_err());
    let lock_path = f
        .manager
        .root()
        .join("views")
        .join(f.view.id())
        .join("workspace-rpc.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .unwrap();
    assert!(FileExt::try_lock_exclusive(&lock).is_err());
    let settled = f.rpc().await_closed(Duration::from_secs(2)).unwrap();
    assert_eq!(
        settled.binding_digest(),
        f.rpc().binding().digest().unwrap()
    );
    assert_eq!(f.rpc().activity().connections, 0);
    assert_eq!(f.rpc().activity().calls, 0);
    FileExt::try_lock_exclusive(&lock).unwrap();
    FileExt::unlock(&lock).unwrap();
    let second = PartialRpc::bind(
        f.view.clone(),
        f.config(),
        &f.endpoint.path().join("second"),
    )
    .unwrap();
    second.await_closed(Duration::from_secs(2)).unwrap();
}

#[test]
fn replacing_named_rpc_lock_refuses_calls_and_settlement_until_exact_custody_is_restored() {
    let mut f = Fixture::new();
    f.start();
    let lock_path = f
        .manager
        .root()
        .join("views")
        .join(f.view.id())
        .join("workspace-rpc.lock");
    let preserved = lock_path.with_extension("preserved");
    fs::rename(&lock_path, &preserved).unwrap();
    let replacement = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    drop(replacement);
    assert!(f.client().call(Operation::Cursor).is_err());
    assert!(
        f.rpc().await_closed(Duration::from_secs(2)).is_err(),
        "different lock identity cannot yield a settled receipt"
    );
    fs::remove_file(&lock_path).unwrap();
    fs::rename(&preserved, &lock_path).unwrap();
    f.rpc().await_closed(Duration::from_secs(2)).unwrap();
}

#[test]
fn work_capacity_cancel_and_close_timeout_wait_for_actual_calls_not_client_disconnection() {
    let mut f = Fixture::new();
    let mut limits = PartialRpcLimits::default();
    limits.max_calls = 1;
    f.start_with(limits);
    // First validation authenticates, second occurs with actual call admission.
    f.authority.block_on.store(
        f.authority.calls.load(Ordering::Acquire) + 2,
        Ordering::Release,
    );
    let client = f.client();
    let id = nonce();
    let for_thread = id.clone();
    let active = thread::spawn(move || {
        client.call_identified(&for_thread, Operation::Cursor, &AtomicBool::new(false))
    });
    until(|| f.authority.entered.load(Ordering::Acquire));
    assert_eq!(f.rpc().activity().calls, 1);
    assert_eq!(
        error_code(&f.client().call(Operation::Cursor).unwrap_err()),
        Some("busy")
    );
    assert_eq!(
        f.client()
            .call(Operation::Cancel { request_id: id })
            .unwrap(),
        Reply::Cancellation {
            request_found: true
        }
    );
    assert!(f.rpc().await_closed(Duration::from_millis(10)).is_err());
    assert_eq!(f.rpc().activity().calls, 1);
    assert!(PartialRpc::bind(
        f.view.clone(),
        f.config(),
        &f.endpoint.path().join("competing")
    )
    .is_err());
    f.authority.release();
    assert!(active.join().unwrap().is_err());
    let settled = f.rpc().await_closed(Duration::from_secs(2)).unwrap();
    assert!(!settled.endpoint_instance().is_empty());
    assert_eq!(f.rpc().activity().connections, 0);
    assert_eq!(f.rpc().activity().calls, 0);
    let next = PartialRpc::bind(
        f.view.clone(),
        f.config(),
        &f.endpoint.path().join("competing"),
    )
    .unwrap();
    next.await_closed(Duration::from_secs(2)).unwrap();
}

#[test]
fn stalled_frames_consume_bounded_connections_and_close_waits_for_accept_completion() {
    let mut f = Fixture::new();
    let mut limits = PartialRpcLimits::default();
    limits.max_connections = 2;
    f.start_with(limits);
    let calls = f.authority.calls.load(Ordering::Acquire);
    let mut first = UnixStream::connect(f.rpc().socket_path()).unwrap();
    first.write_all(&[0, 0]).unwrap();
    let mut second = UnixStream::connect(f.rpc().socket_path()).unwrap();
    second.write_all(&[0, 0]).unwrap();
    until(|| f.rpc().activity().connections == 2);
    let mut rejected = UnixStream::connect(f.rpc().socket_path()).unwrap();
    assert!(response(&mut rejected).is_err());
    assert_eq!(f.authority.calls.load(Ordering::Acquire), calls);
    assert!(f.rpc().activity().connections <= 2);
    f.rpc().await_closed(Duration::from_secs(2)).unwrap();
    assert_eq!(f.rpc().activity().connections, 0);
    assert!(!f.rpc().socket_path().exists());
    assert!(UnixStream::connect(f.rpc().socket_path()).is_err());
    drop((first, second));
}

#[test]
fn dropped_commit_reply_remains_retained_and_reconnect_retry_does_not_recapture() {
    let mut f = Fixture::new();
    f.start();
    f.write(b"original", b"acknowledged source");
    let request = PartialCaptureRequest {
        nonce: nonce(),
        expected_begin: f.cursor(),
        expected_head: None,
    };
    let packet = f.packet(
        serde_json::to_value(Operation::CapturePartial {
            request: OpaqueJson::from_serializable(&request).unwrap(),
        })
        .unwrap(),
    );
    let mut abandoned = UnixStream::connect(f.rpc().socket_path()).unwrap();
    send(&mut abandoned, &packet);
    until(|| {
        f.view
            .inspect_partial_capture(&request.nonce)
            .unwrap()
            .is_some_and(|status| status.receipt.is_some())
    });
    let original = f
        .view
        .inspect_partial_capture(&request.nonce)
        .unwrap()
        .unwrap()
        .receipt
        .unwrap();
    drop(abandoned);
    f.stop();
    f.write(b"new-generation", b"not part of original capture");
    f.start();
    let cursor = f.cursor();
    let pins = f.store.retention().unwrap().pins;
    assert_eq!(f.capture(&request), original);
    assert_eq!(f.cursor(), cursor);
    assert_eq!(f.store.retention().unwrap().pins, pins);
}

#[test]
fn saved_identity_does_not_reopen_an_endpoint_without_fresh_current_partial_authority() {
    let mut f = Fixture::new();
    f.start();
    let identity_path = f.rpc().identity_path();
    let saved = fs::read(&identity_path).unwrap();
    f.stop();
    f.authority.current.store(false, Ordering::Release);
    let error = match PartialRpc::bind(
        f.view.clone(),
        f.config(),
        &f.endpoint.path().join("endpoint"),
    ) {
        Ok(_) => panic!("saved identity revived an unauthorized partial endpoint"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), Some("stale_attempt"));
    assert_eq!(fs::read(&identity_path).unwrap(), saved);
    assert!(!f
        .endpoint
        .path()
        .join("endpoint")
        .join(SOCKET_NAME)
        .exists());
}

#[test]
fn local_view_and_invalid_limits_cannot_create_partial_endpoint_or_hydrate_profile() {
    let f = Fixture::new();
    for mutate in [0, 1, 2, 3] {
        let mut config = f.config();
        match mutate {
            0 => config.limits.max_connections = 0,
            1 => config.limits.max_calls = 5,
            2 => config.limits.capture.work.max_elapsed_ms = 30_001,
            3 => config.limits.capture.delta.max_objects = 0,
            _ => unreachable!(),
        }
        assert!(
            PartialRpc::bind(f.view.clone(), config, &f.endpoint.path().join("invalid")).is_err()
        );
        assert!(!f.endpoint.path().join("invalid").exists());
    }
    let local = VfsManager::open(f._data.path().join("local"), f.store.clone()).unwrap();
    let base = f
        .store
        .put_directory(rudder_engine::store::DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![],
        })
        .unwrap();
    local.prepare(&base).unwrap();
    let view = local
        .fork(
            &nonce(),
            &base,
            OwnerEpoch {
                owner: "local".into(),
                epoch: 1,
            },
        )
        .unwrap();
    assert!(PartialRpc::bind(view, f.config(), &f.endpoint.path().join("local-denied")).is_err());
    assert!(!f.endpoint.path().join("local-denied").exists());
}
