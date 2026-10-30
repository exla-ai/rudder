#![cfg(unix)]
use anyhow::{ensure, Result};
use fs2::FileExt;
use rudder_engine::{
    content,
    directory::{EntryNode, LocalDirectoryStore, PagedDirectory},
    protocol::{canonical_json, ObjectRef, OwnerEpoch},
    store::LocalStore,
    vfs::{rpc::*, NodeKind, ObservationConsistency, VfsManager, VfsObservation, VfsView},
    workspace::WorkspaceProfile,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Fence(AtomicBool, AtomicUsize, AtomicUsize);
impl AttemptAuthority for Fence {
    fn validate(&self, _: &ObservationBinding) -> Result<()> {
        let call = self.1.fetch_add(1, Ordering::AcqRel) + 1;
        ensure!(
            self.0.load(Ordering::Acquire) && call != self.2.load(Ordering::Acquire),
            "retired or temporarily unavailable attempt"
        );
        Ok(())
    }
}
struct Fixture {
    server: Option<ObservationRpc>,
    view: VfsView,
    manager: VfsManager,
    store: LocalStore,
    profile: ObjectRef,
    attempt: AttemptContext,
    fence: Arc<Fence>,
    endpoint: tempfile::TempDir,
    data: tempfile::TempDir,
}
impl Fixture {
    fn new(profile: WorkspaceProfile) -> Self {
        let base = Path::new("/workspace/home/.cache/rudder-vfs-rpc-tests");
        let data = if Path::new("/workspace").is_dir() {
            fs::create_dir_all(base).unwrap();
            tempfile::tempdir_in(base).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        // Explicit short path: Darwin TMPDIR can already exceed sun_path.
        let endpoint = tempfile::Builder::new()
            .prefix("vrpc-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(endpoint.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = LocalStore::open(data.path().join("cas")).unwrap();
        let manager = VfsManager::open(data.path().join("manager"), store.clone()).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let root = PagedDirectory::new(&LocalDirectoryStore::new(&store, &stage.id))
            .build(0o755, [])
            .unwrap();
        store.publish(&stage.id, "fixture", &root).unwrap();
        manager.prepare_paged(&root).unwrap();
        let owner = OwnerEpoch {
            owner: "rpc-fixture".into(),
            epoch: 3,
        };
        let view = manager
            .fork_paged(&Uuid::new_v4().to_string(), &root, owner)
            .unwrap();
        let profile = store.put_blob(&canonical_json(&profile).unwrap()).unwrap();
        Self {
            server: None,
            view,
            manager,
            store,
            profile,
            attempt: AttemptContext {
                execution_id: Uuid::new_v4().to_string(),
                operation_id: Uuid::new_v4().to_string(),
            },
            fence: Arc::new(Fence(
                AtomicBool::new(true),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            )),
            endpoint,
            data,
        }
    }
    fn start(&mut self, witness: Option<Arc<dyn AdoptionWitness>>, limits: RpcLimits) {
        let config = RpcConfig {
            attempt: self.attempt.clone(),
            profile: self.profile.clone(),
            authority: self.fence.clone(),
            adoption_witness: witness,
            limits,
        };
        self.server = Some(
            ObservationRpc::bind(
                self.view.clone(),
                config,
                &self.endpoint.path().join("endpoint"),
            )
            .unwrap(),
        );
    }
    fn rpc(&self) -> &ObservationRpc {
        self.server.as_ref().unwrap()
    }
    fn client(&self) -> ObservationClient {
        ObservationClient::new(
            self.rpc().socket_path(),
            self.rpc().identity_path(),
            self.rpc().binding().clone(),
        )
        .unwrap()
    }
    fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.await_closed(Duration::from_secs(5)).unwrap();
            drop(server);
        }
    }
    fn source(&self, name: &[u8], bytes: &[u8]) -> u64 {
        let file = self
            .view
            .create(1, name, NodeKind::File, 0o644, b"")
            .unwrap();
        self.view.write(file.ino, 0, bytes, false).unwrap();
        file.ino
    }
    fn observe(&self, nonce: &str) -> ObservationReceipt {
        observation(
            self.client()
                .call(Operation::Observe {
                    nonce: nonce.into(),
                })
                .unwrap(),
        )
    }
    fn inspect(&self, nonce: &str) -> ObservationReceipt {
        observation(
            self.client()
                .call(Operation::Inspect {
                    nonce: nonce.into(),
                })
                .unwrap(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = server.await_closed(Duration::from_secs(5));
        }
    }
}
fn observation(reply: Reply) -> ObservationReceipt {
    match reply {
        Reply::Observation(value) => value,
        _ => panic!("expected scoped observation receipt"),
    }
}
fn nonce() -> String {
    Uuid::new_v4().to_string()
}
fn content_at(store: &LocalStore, observation: &VfsObservation, name: &[u8]) -> Vec<u8> {
    let EntryNode::File {
        object,
        logical_size,
        ..
    } = rudder_engine::tree::lookup(store, &observation.directory, name)
        .unwrap()
        .unwrap()
        .node
    else {
        panic!("not a regular file")
    };
    content::read_file_range(store, &object, 0, logical_size as usize).unwrap()
}
fn code(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<RpcError>()
        .map(|error| error.code.as_str())
}
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "bounded fixture condition did not settle"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn request(f: &Fixture, operation: Value) -> Value {
    json!({"format_version":1,"id":nonce(),"identity":read_identity(&f.rpc().identity_path()).unwrap(),"operation":operation})
}
fn send(stream: &mut UnixStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
}
fn response(stream: &mut UnixStream) -> Result<Value> {
    stream.set_read_timeout(Some(Duration::from_millis(20)))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exact = |bytes: &mut [u8]| -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(Instant::now() < deadline, "response deadline");
            match stream.read(&mut bytes[offset..]) {
                Ok(0) => anyhow::bail!("response ended"),
                Ok(n) => offset += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    };
    let mut length = [0; 4];
    exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        length > 0 && length <= MAX_RESPONSE_BYTES,
        "response byte bound"
    );
    let mut body = vec![0; length];
    exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[test]
fn actual_owner_view_source_complement_and_original_nonce_receipts_are_retained() {
    let mut f = Fixture::new(WorkspaceProfile {
        prepared: vec![vec![b"node_modules".to_vec()]],
        vcs_local: vec![vec![b".jj".to_vec()]],
        ..WorkspaceProfile::default()
    });
    let file = f.source(b"source", b"first");
    let dependencies = f
        .view
        .create(1, b"node_modules", NodeKind::Directory, 0o755, b"")
        .unwrap();
    let sparse = f
        .view
        .create(dependencies.ino, b"huge", NodeKind::File, 0o600, b"")
        .unwrap();
    f.view
        .set_attr(sparse.ino, None, Some(1 << 40), None, None)
        .unwrap();
    f.view
        .write(sparse.ino, (1 << 40) - 2, b"XX", false)
        .unwrap();
    f.view
        .create(1, b".jj", NodeKind::Directory, 0o700, b"")
        .unwrap();
    let mut limits = RpcLimits::default();
    // Private blocks are admitted at 64KiB granularity, plus newly encoded
    // content. The excluded 1TiB prepared file cannot fit this capture budget.
    limits.observation.max_content_bytes = 128 * 1024;
    f.start(None, limits);
    assert!(
        f.manager
            .open_view(f.view.id(), &f.rpc().binding().owner)
            .is_err(),
        "RPC did not reopen the exclusively owned view"
    );
    let key = nonce();
    let first = f.observe(&key);
    assert_eq!(first.state, ReceiptState::Retained);
    assert!(first.retained && !first.current);
    let observed = first.observation.as_ref().unwrap();
    assert_eq!(observed.consistency, ObservationConsistency::LiveBestEffort);
    assert_eq!(observed.begin.namespace, f.rpc().binding().namespace);
    assert_eq!(observed.begin.owner, f.rpc().binding().owner);
    assert_eq!(content_at(&f.store, observed, b"source"), b"first");
    assert!(
        rudder_engine::tree::lookup(&f.store, &observed.directory, b"node_modules")
            .unwrap()
            .is_none()
    );
    assert!(
        rudder_engine::tree::lookup(&f.store, &observed.directory, b".jj")
            .unwrap()
            .is_none()
    );
    f.view.write(file, 0, b"later", false).unwrap();
    assert_eq!(
        f.observe(&key),
        first,
        "reconnect retry returns the original retained root"
    );
    let Reply::Cursor(cursor) = f.client().call(Operation::Cursor).unwrap() else {
        panic!()
    };
    assert!(cursor.generation > observed.end.generation);
    f.store.gc(Duration::ZERO).unwrap();
    assert_eq!(content_at(&f.store, observed, b"source"), b"first");
    f.stop();
    f.start(None, RpcLimits::default());
    assert_eq!(
        f.inspect(&key),
        first,
        "server restart reuses durable attempt-scoped receipt"
    );
}

#[test]
fn authenticity_attempt_binding_unknown_operations_and_identity_files_fail_before_effects() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    f.source(b"source", b"safe");
    f.start(None, RpcLimits::default());
    let before = f.store.retention().unwrap().pins;
    for field in [
        "token",
        "view",
        "owner",
        "namespace",
        "profile",
        "execution",
    ] {
        let mut value = request(&f, json!({"operation":"observe","nonce":nonce()}));
        match field {
            "token" => value["identity"]["token"] = json!("f".repeat(64)),
            "view" => value["identity"]["binding"]["view_id"] = json!(nonce()),
            "owner" => value["identity"]["binding"]["owner"]["epoch"] = json!(4),
            "namespace" => value["identity"]["binding"]["namespace"] = json!(nonce()),
            "profile" => value["identity"]["binding"]["profile"]["digest"] = json!("a".repeat(64)),
            _ => value["identity"]["binding"]["attempt"]["execution_id"] = json!(nonce()),
        }
        let mut stream = UnixStream::connect(f.rpc().socket_path()).unwrap();
        send(&mut stream, &value);
        assert_eq!(
            response(&mut stream).unwrap()["error"]["code"],
            "unauthenticated"
        );
    }
    for operation in [
        json!({"operation":"seal"}),
        json!({"operation":"observe","nonce":nonce(),"excluded_paths":[["source"]]}),
        json!({"operation":"cursor","cas_root":"/private/authority"}),
    ] {
        let mut stream = UnixStream::connect(f.rpc().socket_path()).unwrap();
        send(&mut stream, &request(&f, operation));
        assert!(response(&mut stream).is_err());
    }
    assert_eq!(f.store.retention().unwrap().pins, before);
    f.view
        .write(
            f.view.lookup(1, b"source").unwrap().ino,
            0,
            b"still mutable",
            false,
        )
        .unwrap();
    let bad = f.endpoint.path().join("public-identity.json");
    fs::copy(f.rpc().identity_path(), &bad).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        ObservationClient::new(f.rpc().socket_path(), &bad, f.rpc().binding().clone()).is_err()
    );
    f.fence.0.store(false, Ordering::Release);
    assert_eq!(
        code(&f.client().call(Operation::Cursor).unwrap_err()),
        Some("stale_attempt")
    );
}

#[test]
fn release_is_idempotent_and_never_operates_on_another_attempt_or_direct_owner_pin() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    f.source(b"source", b"content");
    let foreign = nonce();
    let direct = f
        .view
        .observe_source(&Default::default(), &foreign, &Default::default())
        .unwrap();
    f.start(None, RpcLimits::default());
    for operation in [
        Operation::Inspect {
            nonce: foreign.clone(),
        },
        Operation::Release {
            nonce: foreign.clone(),
        },
        Operation::Observe {
            nonce: foreign.clone(),
        },
    ] {
        assert!(f.client().call(operation).is_err());
    }
    assert_eq!(
        f.store.retention().unwrap().pins.get(&direct.pin_id),
        Some(&direct.directory)
    );
    let key = nonce();
    let initial = f.observe(&key);
    let released = observation(
        f.client()
            .call(Operation::Release { nonce: key.clone() })
            .unwrap(),
    );
    assert_eq!(released.state, ReceiptState::Released);
    assert!(!released.retained && !released.current);
    assert_eq!(
        observation(
            f.client()
                .call(Operation::Release { nonce: key.clone() })
                .unwrap()
        ),
        released
    );
    assert!(f
        .client()
        .call(Operation::Observe { nonce: key.clone() })
        .is_err());
    assert!(initial.retained);
    let old = f.client();
    f.stop();
    f.attempt.execution_id = nonce();
    // A different attempt cannot silently replace the projected identity.
    let config = RpcConfig {
        attempt: f.attempt.clone(),
        profile: f.profile.clone(),
        authority: f.fence.clone(),
        adoption_witness: None,
        limits: RpcLimits::default(),
    };
    assert!(
        ObservationRpc::bind(f.view.clone(), config, &f.endpoint.path().join("endpoint")).is_err()
    );
    assert!(old.call(Operation::Cursor).is_err());
}

#[test]
fn adoption_without_a_trusted_witness_is_unsupported_and_preserves_both_roots() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    let file = f.source(b"source", b"one");
    f.start(None, RpcLimits::default());
    let a = f.observe(&nonce());
    f.view.write(file, 0, b"two", false).unwrap();
    let b = f.observe(&nonce());
    let error = f
        .client()
        .call(Operation::Adopt {
            operation_id: nonce(),
            expected_current: None,
            nonce: b.nonce.clone(),
            evidence: AdoptionEvidence {
                state_sha256: "a".repeat(64),
                tree: json!({"fsynced":true}),
            },
        })
        .unwrap_err();
    assert_eq!(code(&error), Some("unsupported"));
    assert!(f.inspect(&a.nonce).retained && f.inspect(&b.nonce).retained);
    assert_eq!(f.client().call(Operation::Head).unwrap(), Reply::Head(None));
}

struct FileGuard(File);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
#[derive(Serialize, Deserialize)]
struct WcState {
    binding: ObservationBinding,
    observation: VfsObservation,
    tree: Value,
}
struct WorkingCopyWitness {
    directory: PathBuf,
}
impl WorkingCopyWitness {
    fn new(path: &Path) -> Arc<Self> {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        Arc::new(Self {
            directory: path.to_path_buf(),
        })
    }
    fn commit(
        &self,
        binding: &ObservationBinding,
        observation: &VfsObservation,
        tree: Value,
    ) -> AdoptionEvidence {
        let bytes = canonical_json(&WcState {
            binding: binding.clone(),
            observation: observation.clone(),
            tree: tree.clone(),
        })
        .unwrap();
        let temporary = self.directory.join("state.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        fs::rename(&temporary, self.directory.join("state.json")).unwrap();
        File::open(&self.directory).unwrap().sync_all().unwrap();
        AdoptionEvidence {
            state_sha256: hex::encode(Sha256::digest(bytes)),
            tree,
        }
    }
}
impl AdoptionWitness for WorkingCopyWitness {
    fn verify<'a>(
        &'a self,
        binding: &ObservationBinding,
        observation: &VfsObservation,
        evidence: &AdoptionEvidence,
    ) -> Result<Box<dyn AdoptionGuard + 'a>> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("state.lock"))?;
        FileExt::try_lock_exclusive(&lock)?;
        let guard = FileGuard(lock);
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("state.json"))?;
        ensure!(file.metadata()?.len() <= 32 * 1024, "private state bound");
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(32 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            hex::encode(Sha256::digest(&bytes)) == evidence.state_sha256,
            "WC state hash was not durably published"
        );
        let state: WcState = serde_json::from_slice(&bytes)?;
        ensure!(
            state.binding == *binding
                && state.observation == *observation
                && state.tree == evidence.tree,
            "WC state/observation/tree differs"
        );
        file.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        Ok(Box::new(guard))
    }
}
fn adopt(
    client: &ObservationClient,
    operation_id: &str,
    expected: Option<&str>,
    nonce: &str,
    evidence: AdoptionEvidence,
) -> Result<AdoptionReceipt> {
    match client.call(Operation::Adopt {
        operation_id: operation_id.into(),
        expected_current: expected.map(str::to_string),
        nonce: nonce.into(),
        evidence,
    })? {
        Reply::Adoption(receipt) => Ok(receipt),
        _ => anyhow::bail!("expected durable adoption receipt"),
    }
}

#[test]
fn actual_private_wc_fsync_witness_protects_current_and_releases_only_replaced_roots() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    let file = f.source(b"source", b"one");
    let witness = WorkingCopyWitness::new(&f.data.path().join("private-wc"));
    f.start(Some(witness.clone()), RpcLimits::default());
    let first = f.observe(&nonce());
    let a = first.observation.as_ref().unwrap();
    let evidence_a = witness.commit(f.rpc().binding(), a, json!({"resolved":"first-jj-tree"}));
    let first_operation = nonce();
    let accepted_a = adopt(
        &f.client(),
        &first_operation,
        None,
        &first.nonce,
        evidence_a.clone(),
    )
    .unwrap();
    assert_eq!(accepted_a.sequence, 1);
    assert!(f.inspect(&first.nonce).current);
    assert!(f
        .client()
        .call(Operation::Release {
            nonce: first.nonce.clone()
        })
        .is_err());
    f.view.write(file, 0, b"two", false).unwrap();
    let second = f.observe(&nonce());
    let evidence_b = witness.commit(
        f.rpc().binding(),
        second.observation.as_ref().unwrap(),
        json!({"resolved":"second-jj-tree"}),
    );
    assert!(
        f.inspect(&first.nonce).retained && f.inspect(&second.nonce).retained,
        "WC state publication alone cannot release an engine pin"
    );
    let second_operation = nonce();
    let accepted_b = adopt(
        &f.client(),
        &second_operation,
        Some(&first.nonce),
        &second.nonce,
        evidence_b.clone(),
    )
    .unwrap();
    assert_eq!(accepted_b.sequence, 2);
    assert_eq!(f.inspect(&first.nonce).state, ReceiptState::Released);
    assert!(!f.inspect(&first.nonce).retained);
    assert!(f.inspect(&second.nonce).current && f.inspect(&second.nonce).retained);
    assert_eq!(
        adopt(
            &f.client(),
            &first_operation,
            None,
            &first.nonce,
            evidence_a
        )
        .unwrap(),
        accepted_a,
        "historic retry returns original receipt without reverting current"
    );
    assert_eq!(
        adopt(
            &f.client(),
            &second_operation,
            Some(&first.nonce),
            &second.nonce,
            evidence_b
        )
        .unwrap(),
        accepted_b
    );
    assert_eq!(
        f.client().call(Operation::Head).unwrap(),
        Reply::Head(Some(accepted_b))
    );
    let bad = AdoptionEvidence {
        state_sha256: "0".repeat(64),
        tree: json!("forged"),
    };
    assert!(adopt(
        &f.client(),
        &second_operation,
        Some(&first.nonce),
        &second.nonce,
        bad
    )
    .is_err());
    assert!(f.inspect(&second.nonce).retained);
}

struct RaceWitness {
    arrivals: Mutex<usize>,
    ready: Condvar,
    allowed: BTreeEvidence,
}
type BTreeEvidence = std::collections::BTreeSet<String>;
impl AdoptionWitness for RaceWitness {
    fn verify<'a>(
        &'a self,
        _: &ObservationBinding,
        observation: &VfsObservation,
        evidence: &AdoptionEvidence,
    ) -> Result<Box<dyn AdoptionGuard + 'a>> {
        ensure!(
            self.allowed.contains(&observation.request_nonce)
                && evidence.state_sha256 == "a".repeat(64),
            "not independently authorized test state"
        );
        let mut arrivals = self.arrivals.lock().unwrap();
        *arrivals += 1;
        self.ready.notify_all();
        let (arrivals, _) = self
            .ready
            .wait_timeout_while(arrivals, Duration::from_secs(5), |arrivals| *arrivals < 2)
            .unwrap();
        ensure!(*arrivals == 2, "concurrent witness rendezvous timed out");
        Ok(Box::new(()))
    }
}
#[test]
fn competing_durable_adoptions_compare_the_head_in_the_transaction_and_never_unpin_the_loser() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    let file = f.source(b"source", b"one");
    f.start(None, RpcLimits::default());
    let a = f.observe(&nonce());
    f.view.write(file, 0, b"two", false).unwrap();
    let b = f.observe(&nonce());
    f.stop();
    let witness = Arc::new(RaceWitness {
        arrivals: Mutex::new(0),
        ready: Condvar::new(),
        allowed: [a.nonce.clone(), b.nonce.clone()].into_iter().collect(),
    });
    f.start(Some(witness), RpcLimits::default());
    let operations = [a.nonce.clone(), b.nonce.clone()]
        .into_iter()
        .map(|key| {
            let client = f.client();
            thread::spawn(move || {
                adopt(
                    &client,
                    &nonce(),
                    None,
                    &key,
                    AdoptionEvidence {
                        state_sha256: "a".repeat(64),
                        tree: json!({"state":key}),
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    let results = operations
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let Reply::Head(Some(current)) = f.client().call(Operation::Head).unwrap() else {
        panic!()
    };
    assert_eq!(current.sequence, 1);
    assert!(f.inspect(&a.nonce).retained && f.inspect(&b.nonce).retained);
}

#[test]
fn dirty_pages_over_bsd_socket_buffers_survive_half_close_and_do_not_acknowledge_or_seal() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    f.start(None, RpcLimits::default());
    let Reply::Cursor(before) = f.client().call(Operation::Cursor).unwrap() else {
        panic!()
    };
    for index in 0..160 {
        let name = format!("{index:04}-{}", "x".repeat(240));
        f.view
            .create(1, name.as_bytes(), NodeKind::File, 0o644, b"")
            .unwrap();
    }
    let mut stream = UnixStream::connect(f.rpc().socket_path()).unwrap();
    send(
        &mut stream,
        &request(
            &f,
            serde_json::to_value(Operation::Dirty {
                after: before.clone(),
                limit: MAX_DIRTY_EVENTS,
            })
            .unwrap(),
        ),
    );
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let reply = response(&mut stream).unwrap();
    assert!(
        serde_json::to_vec(&reply).unwrap().len() > 50_000,
        "fixture must exceed BSD socket buffers"
    );
    let Reply::Dirty(page) = serde_json::from_value(reply["result"].clone()).unwrap() else {
        panic!()
    };
    assert_eq!(page.events.len(), MAX_DIRTY_EVENTS);
    assert!(page.has_more && !page.complete && !page.cursor_lost);
    assert!(page
        .events
        .windows(2)
        .all(|events| events[1].generation == events[0].generation + 1));
    let Reply::Dirty(tail) = f
        .client()
        .call(Operation::Dirty {
            after: page.next,
            limit: MAX_DIRTY_EVENTS,
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(tail.complete && !tail.cursor_lost && !tail.has_more);
    assert_eq!(tail.next, tail.current);
    assert!(f
        .client()
        .call(Operation::Dirty {
            after: before,
            limit: MAX_DIRTY_EVENTS + 1
        })
        .is_err());
    assert_eq!(f.view.dirty_cursor().unwrap(), tail.current);
    f.source(b"after-rpc", b"still writable");
}

#[test]
fn connection_frame_retention_and_capture_budgets_are_enforced_before_new_effects() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    f.source(b"source", b"a");
    let mut limits = RpcLimits::default();
    limits.max_connections = 1;
    limits.max_live_observations = 1;
    limits.max_receipts = 2;
    f.start(None, limits);
    let mut stalled = UnixStream::connect(f.rpc().socket_path()).unwrap();
    stalled.write_all(&[0]).unwrap();
    until(|| f.rpc().activity().connections == 1);
    assert!(f.client().call(Operation::Cursor).is_err());
    drop(stalled);
    until(|| f.rpc().activity().connections == 0);
    for body in [vec![0xff], b"{}".to_vec()] {
        let mut stream = UnixStream::connect(f.rpc().socket_path()).unwrap();
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&body).unwrap();
        assert!(response(&mut stream).is_err());
        until(|| f.rpc().activity().connections == 0);
    }
    let mut too_large = UnixStream::connect(f.rpc().socket_path()).unwrap();
    too_large
        .write_all(&((MAX_REQUEST_BYTES + 1) as u32).to_be_bytes())
        .unwrap();
    assert!(response(&mut too_large).is_err());
    until(|| f.rpc().activity().connections == 0);
    let a = f.observe(&nonce());
    assert!(f
        .client()
        .call(Operation::Observe { nonce: nonce() })
        .is_err());
    f.client()
        .call(Operation::Release { nonce: a.nonce })
        .unwrap();
    let b = f.observe(&nonce());
    f.client()
        .call(Operation::Release { nonce: b.nonce })
        .unwrap();
    assert!(
        f.client()
            .call(Operation::Observe { nonce: nonce() })
            .is_err(),
        "released tombstones must not be evicted to replay an old nonce"
    );
    f.stop();
    let mut limits = RpcLimits::default();
    limits.observation.max_content_bytes = 0;
    f.start(None, limits);
    let before = f.store.retention().unwrap().pins;
    assert!(f
        .client()
        .call(Operation::Observe { nonce: nonce() })
        .is_err());
    assert_eq!(f.store.retention().unwrap().pins, before);
}

struct GateWitness {
    wc: Arc<WorkingCopyWitness>,
    entered: Arc<AtomicBool>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}
impl AdoptionWitness for GateWitness {
    fn verify<'a>(
        &'a self,
        binding: &ObservationBinding,
        observation: &VfsObservation,
        evidence: &AdoptionEvidence,
    ) -> Result<Box<dyn AdoptionGuard + 'a>> {
        let guard = self.wc.verify(binding, observation, evidence)?;
        self.entered.store(true, Ordering::Release);
        let open = self.gate.0.lock().unwrap();
        let (open, _) = self
            .gate
            .1
            .wait_timeout_while(open, Duration::from_secs(5), |open| !*open)
            .unwrap();
        ensure!(*open, "test witness gate timed out");
        Ok(guard)
    }
}
#[test]
fn call_admission_explicit_cancel_and_close_wait_for_settlement_without_false_adoption() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    f.source(b"source", b"one");
    let wc = WorkingCopyWitness::new(&f.data.path().join("wc"));
    let entered = Arc::new(AtomicBool::new(false));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let witness = Arc::new(GateWitness {
        wc: wc.clone(),
        entered: entered.clone(),
        gate: gate.clone(),
    });
    let mut limits = RpcLimits::default();
    limits.max_calls = 1;
    f.start(Some(witness), limits);
    let observation = f.observe(&nonce());
    let evidence = wc.commit(
        f.rpc().binding(),
        observation.observation.as_ref().unwrap(),
        json!("durable-tree"),
    );
    let client = f.client();
    let request_id = nonce();
    let request_for_thread = request_id.clone();
    let key = observation.nonce.clone();
    let active = thread::spawn(move || {
        client.call_identified(
            &request_for_thread,
            Operation::Adopt {
                operation_id: nonce(),
                expected_current: None,
                nonce: key,
                evidence,
            },
            &AtomicBool::new(false),
        )
    });
    until(|| entered.load(Ordering::Acquire));
    assert_eq!(f.rpc().activity().calls, 1);
    assert_eq!(
        code(&f.client().call(Operation::Cursor).unwrap_err()),
        Some("busy")
    );
    assert_eq!(
        f.client().call(Operation::Cancel { request_id }).unwrap(),
        Reply::Cancellation {
            request_found: true
        }
    );
    assert!(
        f.rpc().await_closed(Duration::from_millis(20)).is_err(),
        "closing cannot pretend the blocked trusted witness settled"
    );
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(active.join().unwrap().is_err());
    f.stop();
    f.start(None, RpcLimits::default());
    assert_eq!(f.client().call(Operation::Head).unwrap(), Reply::Head(None));
    assert!(f.inspect(&observation.nonce).retained && !f.inspect(&observation.nonce).current);
    let cancel = AtomicBool::new(true);
    let before = f.store.retention().unwrap().pins;
    assert_eq!(
        code(
            &f.client()
                .call_with_cancel(Operation::Observe { nonce: nonce() }, &cancel)
                .unwrap_err()
        ),
        Some("cancelled")
    );
    assert_eq!(f.store.retention().unwrap().pins, before);
}

#[test]
fn dropped_observation_response_is_still_retained_after_restart_until_explicit_release() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    f.source(b"source", b"unacknowledged");
    f.start(None, RpcLimits::default());
    let key = nonce();
    let mut unobserved = UnixStream::connect(f.rpc().socket_path()).unwrap();
    send(
        &mut unobserved,
        &request(&f, json!({"operation":"observe","nonce":key})),
    );
    until(|| {
        f.client()
            .call(Operation::Inspect { nonce: key.clone() })
            .ok()
            .map(observation)
            .is_some_and(|receipt| receipt.state == ReceiptState::Retained)
    });
    drop(unobserved);
    f.stop();
    f.start(None, RpcLimits::default());
    let retained = f.inspect(&key);
    assert!(retained.retained && !retained.current);
    f.store.gc(Duration::ZERO).unwrap();
    assert_eq!(
        content_at(&f.store, retained.observation.as_ref().unwrap(), b"source"),
        b"unacknowledged"
    );
    f.client().call(Operation::Release { nonce: key }).unwrap();
}

#[test]
fn only_one_rpc_owns_the_actual_view_and_settled_close_releases_both_locks() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    f.start(None, RpcLimits::default());
    let config = || RpcConfig {
        attempt: f.attempt.clone(),
        profile: f.profile.clone(),
        authority: f.fence.clone(),
        adoption_witness: None,
        limits: RpcLimits::default(),
    };
    assert!(
        ObservationRpc::bind(f.view.clone(), config(), &f.endpoint.path().join("other")).is_err(),
        "a different socket path cannot duplicate a mounted view's nonce authority"
    );
    let old = f.server.take().unwrap();
    old.await_closed(Duration::from_secs(5)).unwrap();
    // Keep the old handle alive: releasing ownership must follow settled calls,
    // not the unspecified scheduling of the last old Arc's destructor.
    f.start(None, RpcLimits::default());
    assert!(matches!(
        f.client().call(Operation::Cursor).unwrap(),
        Reply::Cursor(_)
    ));
    drop(old);
    assert!(
        f.rpc().socket_path().exists(),
        "old owner cannot unlink its replacement socket"
    );
}

#[test]
fn failed_retirement_after_durable_adoption_reconciles_after_restart_without_reverting_head() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    let file = f.source(b"source", b"first");
    let witness = WorkingCopyWitness::new(&f.data.path().join("private-wc"));
    f.start(Some(witness.clone()), RpcLimits::default());
    let a = f.observe(&nonce());
    let evidence_a = witness.commit(
        f.rpc().binding(),
        a.observation.as_ref().unwrap(),
        json!("tree-a"),
    );
    adopt(&f.client(), &nonce(), None, &a.nonce, evidence_a).unwrap();
    f.view.write(file, 0, b"later", false).unwrap();
    let b = f.observe(&nonce());
    let evidence_b = witness.commit(
        f.rpc().binding(),
        b.observation.as_ref().unwrap(),
        json!("tree-b"),
    );
    let operation = nonce();
    // Authenticate, validate view, then validate after trusted witness; the next
    // authority read is retirement cleanup AFTER the head transaction committed.
    f.fence
        .2
        .store(f.fence.1.load(Ordering::Acquire) + 4, Ordering::Release);
    let receipt = adopt(
        &f.client(),
        &operation,
        Some(&a.nonce),
        &b.nonce,
        evidence_b.clone(),
    )
    .unwrap();
    let pending = f.inspect(&a.nonce);
    assert_eq!(pending.state, ReceiptState::Retiring);
    assert!(pending.retained && !pending.current);
    assert!(f.inspect(&b.nonce).current && f.inspect(&b.nonce).retained);
    f.stop();
    f.store.gc(Duration::ZERO).unwrap();
    f.start(Some(witness), RpcLimits::default());
    assert_eq!(
        adopt(
            &f.client(),
            &operation,
            Some(&a.nonce),
            &b.nonce,
            evidence_b
        )
        .unwrap(),
        receipt
    );
    assert_eq!(f.inspect(&a.nonce).state, ReceiptState::Released);
    assert!(!f.inspect(&a.nonce).retained);
    assert!(f.inspect(&b.nonce).current && f.inspect(&b.nonce).retained);
    assert_eq!(
        f.client().call(Operation::Head).unwrap(),
        Reply::Head(Some(receipt))
    );
}

#[test]
fn source_observations_serialize_logical_writes_without_claiming_writer_quiescence() {
    let mut f = Fixture::new(WorkspaceProfile::default());
    let file = f.source(b"source", &[b'A'; 64]);
    f.start(None, RpcLimits::default());
    let view = f.view.clone();
    let writer = thread::spawn(move || {
        for index in 0..80 {
            view.write(
                file,
                0,
                &[if index % 2 == 0 { b'B' } else { b'A' }; 64],
                false,
            )
            .unwrap();
            thread::yield_now();
        }
    });
    for _ in 0..8 {
        let observed = f.observe(&nonce()).observation.unwrap();
        let bytes = content_at(&f.store, &observed, b"source");
        assert!(bytes == [b'A'; 64] || bytes == [b'B'; 64]);
        assert_eq!(
            observed.begin, observed.end,
            "logical view mutex brackets a capture"
        );
        assert_eq!(
            observed.consistency,
            ObservationConsistency::LiveBestEffort,
            "matching cursors do not prove mmap writers drained or a namespace stopped"
        );
    }
    writer.join().unwrap();
    f.view.write(file, 0, b"after observation", false).unwrap();
}

#[test]
fn operation_schema_rejects_duplicate_fields_and_unknown_unit_variant_fields() {
    for bytes in [
        r#"{"operation":"cursor","root":"ignored"}"#,
        r#"{"operation":"head","fsynced":true}"#,
        r#"{"operation":"cursor","operation":"head"}"#,
        r#"{"operation":"observe","nonce":"a","nonce":"b"}"#,
        r#"{"operation":"adopt","operation_id":"x","nonce":"y","evidence":{"state_sha256":"x","tree":null,"fsynced":true}}"#,
    ] {
        assert!(
            serde_json::from_str::<Operation>(bytes).is_err(),
            "accepted {bytes}"
        );
    }
}
