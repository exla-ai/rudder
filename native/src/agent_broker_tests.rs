use super::*;
use rudder_engine::journal::{CapabilityGrant, Journal, OwnerFence};
use std::collections::{BTreeMap, BTreeSet};

fn partial_workspace_binding(f:&Fixture,execution_id:&str)->rudder_engine::vfs::partial_rpc::PartialBinding {
    use rudder_engine::{remote_cache::CacheScope,remote_root::{AttemptBinding,RootBinding},protocol::{ObjectKind,ObjectRef},store::ImportPolicy,vfs::{exclusion_policy_digest,partial_rpc::{PartialBinding,PROTOCOL}}};
    let mut observation=workspace_binding(f,execution_id);
    observation.exclusion_policy_digest=exclusion_policy_digest(&ImportPolicy::default()).unwrap();
    let attempt=AttemptBinding{project_id:"p".into(),run_id:"a".into(),run_epoch:1,owner:observation.owner.clone(),execution_id:execution_id.into(),operation_id:observation.attempt.operation_id.clone(),view_id:observation.view_id.clone()};
    PartialBinding{protocol:PROTOCOL.into(),format_version:1,launch_request_digest:"a".repeat(64),root:RootBinding{scope:CacheScope{provider_id:"broker-fixture".into(),tenant_id:uuid::Uuid::new_v4().to_string()},root_id:"root".into(),root:ObjectRef::from_bytes(ObjectKind::RunnableRoot,b"fixture")},attempt,observation}
}

#[test]
fn partial_workspace_broker_requires_new_right_exact_scope_and_current_instance() {
    use rudder_engine::vfs::partial_rpc::{partial_scope,REQUIRED_OPERATION};
    let mut f=Fixture::new();let execution=uuid::Uuid::new_v4().to_string();f.journal.observe_run_attempt(&f.owner,"a",&execution,0).unwrap();
    let partial=partial_workspace_binding(&f,&execution);let _broker=f.broker();let client=f.client();assert!(client.authorize_partial_workspace(&partial).is_err());
    let mut complete=grant("a");complete.operations.insert("workspace:observe".into());complete.scopes.insert(rudder_engine::execution::workspace_scope(&partial.observation).unwrap());
    complete.scopes.insert(partial_scope(&partial).unwrap());f.a.token=f.journal.issue_capability(&f.owner,&complete,0).unwrap();project_identity(&f.socket,&f.a).unwrap();
    assert!(client.authorize_partial_workspace(&partial).is_err(),"complete right must not authorize partial scope");client.authorize_workspace(&partial.observation).unwrap();
    let mut allowed=grant("a");allowed.operations.insert(REQUIRED_OPERATION.into());f.a.token=f.journal.issue_capability(&f.owner,&allowed,0).unwrap();project_identity(&f.socket,&f.a).unwrap();
    assert!(client.authorize_partial_workspace(&partial).is_err(),"partial operation alone does not authorize a view");
    allowed.scopes.insert(partial_scope(&partial).unwrap());f.a.token=f.journal.issue_capability(&f.owner,&allowed,0).unwrap();project_identity(&f.socket,&f.a).unwrap();
    let bound=BrokerClient::connect_partial_workspace(f.socket.clone(),f.socket.with_file_name("identity.json"),&BrokerBinding::from(&f.a),&partial).unwrap();
    bound.authorize_partial_workspace(&partial).unwrap();assert!(bound.authorize_workspace(&partial.observation).is_err());
    let mut foreign=partial.clone();foreign.launch_request_digest="f".repeat(64);assert!(bound.authorize_partial_workspace(&foreign).is_err());
    f.journal.observe_run_attempt(&f.owner,"a",&uuid::Uuid::new_v4().to_string(),1).unwrap();assert!(bound.authorize_partial_workspace(&partial).is_err());
}

#[test]
fn partial_workspace_authorization_keeps_two_second_saturated_connect_budget() {
    let f = Fixture::new();
    let (listener, held) = saturated_unaccepted_backlog(&f.socket);
    let partial = partial_workspace_binding(&f, &uuid::Uuid::new_v4().to_string());
    let start = Instant::now();
    let error = BrokerClient::connect_partial_workspace(
        f.socket.clone(), f.socket.with_file_name("identity.json"),
        &BrokerBinding::from(&f.a), &partial,
    ).err().expect("a saturated listener must not authorize partial workspace access");
    assert_eq!(error.downcast_ref::<WireError>().map(|error| error.code.as_str()), Some("unavailable"));
    assert!(start.elapsed() < Duration::from_secs(3), "partial auth used generic long-poll connect budget");
    drop(held); drop(listener);
}

fn saturated_unaccepted_backlog(path: &Path) -> (UnixListener, Vec<mio::net::UnixStream>) {
    let listener = UnixListener::bind(path).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
    listener.set_nonblocking(true).unwrap();
    let mut held = Vec::new();
    let mut saturated = false;
    for _ in 0..16 {
        match mio::net::UnixStream::connect(path) {
            Ok(stream) => held.push(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                saturated = true; break;
            }
            // XNU's unp_connect returns ECONNREFUSED when sonewconn cannot
            // allocate another accept-queue entry. Linux reports EAGAIN.
            #[cfg(target_os = "macos")]
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                saturated = true; break;
            }
            Err(error) => panic!("fill private socket backlog: {error}"),
        }
    }
    assert!(!held.is_empty(), "fixture must first establish a queued connection");
    assert!(saturated, "fixture must observe the terminal full-backlog error");
    (listener, held)
}

#[test]
fn broker_identity_fifo_is_rejected_without_waiting_for_writer() {
    use std::os::unix::ffi::OsStrExt;
    let f=Fixture::new();let fifo=f.root.join("fifo-identity");let name=std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);
    let (send,receive)=std::sync::mpsc::channel();let path=fifo.clone();let reader=std::thread::spawn(move||{let _=send.send(read_identity(&path).is_err());});
    let prompt=receive.recv_timeout(Duration::from_secs(1));if prompt.is_err(){let _writer=OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&fifo);let _=receive.recv_timeout(Duration::from_secs(1));}
    if reader.is_finished(){reader.join().unwrap();}assert_eq!(prompt.ok(),Some(true));
}

struct Fixture {
    root: PathBuf,
    journal: Journal,
    owner: OwnerFence,
    a: AgentIdentityFile,
    b: AgentIdentityFile,
    socket: PathBuf,
    callbacks: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
    keep: bool,
}
fn grant(run: &str) -> CapabilityGrant {
    CapabilityGrant {
        principal: format!("peer-{run}"),
        run_id: run.into(),
        run_epoch: 1,
        operations: [
            "agents:find",
            "message:send",
            "mailbox:read",
            "mailbox:ack",
            "command:steer",
            "command:interrupt",
            "command:resume",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        recipients: BTreeMap::from([("a".into(), 1), ("b".into(), 1)]),
        scopes: BTreeSet::from(["project".into()]),
        topics: BTreeSet::new(),
        expires_at_ms: None,
    }
}
impl Fixture {
    fn new() -> Self {
        Self::at(
            std::env::temp_dir().join(format!("rudder-broker-test-{}", uuid::Uuid::new_v4())),
            false,
        )
    }
    fn at(root: PathBuf, keep: bool) -> Self {
        private_directory(&root).unwrap();
        let mut journal = Journal::open(root.join("journal.sqlite")).unwrap();
        let owner = journal.create_project("p", "engine", "base", 0).unwrap();
        for run in ["a", "b"] {
            journal.register_run(&owner, run, 0).unwrap();
        }
        let a = AgentIdentityFile {
            format_version: 1,
            project_id: "p".into(),
            run_id: "a".into(),
            run_epoch: 1,
            owner_epoch: 1,
            token: journal.issue_capability(&owner, &grant("a"), 0).unwrap(),
        };
        let b = AgentIdentityFile {
            run_id: "b".into(),
            token: journal.issue_capability(&owner, &grant("b"), 0).unwrap(),
            ..a.clone()
        };
        let socket = socket_path(&root, "p", "a", "attempt-1");
        project_identity(&socket, &a).unwrap();
        Self {
            root,
            journal,
            owner,
            a,
            b,
            socket,
            callbacks: Mutex::new(Vec::new()),
            keep,
        }
    }
    fn config(&self) -> BrokerConfig {
        BrokerConfig {
            journal_path: self.root.join("journal.sqlite"),
            identity_file: self.socket.with_file_name("identity.json"),
            socket_path: self.socket.clone(),
        }
    }
    fn broker(&self) -> AgentBroker {
        let broker = AgentBroker::spawn(self.config()).unwrap();
        self.callbacks.lock().unwrap().push(broker.close_callback());
        broker
    }
    fn client(&self) -> BrokerClient {
        BrokerClient::connect(
            self.socket.clone(),
            self.socket.with_file_name("identity.json"),
        )
        .unwrap()
    }
    fn rotate(&mut self) -> AgentIdentityFile {
        let next = AgentIdentityFile {
            token: self
                .journal
                .issue_capability(&self.owner, &grant("a"), 0)
                .unwrap(),
            ..self.a.clone()
        };
        project_identity(&self.socket, &next).unwrap();
        next
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for close in self.callbacks.lock().unwrap().iter() {
            close();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !self.keep {
            let _ = fs::remove_dir_all(self.socket.parent().unwrap());
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
fn call(client: &BrokerClient, name: &str, args: Value) -> Result<Value> {
    client.call_with_cancel(name, args, &AtomicBool::new(false))
}

fn workspace_binding(f: &Fixture, execution_id: &str) -> rudder_engine::vfs::rpc::ObservationBinding {
    use rudder_engine::protocol::{ObjectKind, ObjectRef, OwnerEpoch};
    use rudder_engine::vfs::rpc::{AttemptContext, ObservationBinding};
    ObservationBinding {
        format_version: 1,
        attempt: AttemptContext { execution_id: execution_id.into(), operation_id: uuid::Uuid::new_v4().to_string() },
        view_id: uuid::Uuid::new_v4().to_string(),
        owner: OwnerEpoch { owner: f.owner.owner_id.clone(), epoch: f.owner.epoch },
        namespace: uuid::Uuid::new_v4().to_string(),
        profile: ObjectRef::from_bytes(ObjectKind::Blob, b"{}"),
        exclusion_policy_digest: "a".repeat(64),
    }
}

#[test]
fn workspace_authorization_requires_explicit_scope_and_current_execution() {
    let mut f = Fixture::new();
    let execution_id = uuid::Uuid::new_v4().to_string();
    f.journal.observe_run_attempt(&f.owner, "a", &execution_id, 0).unwrap();
    let binding = workspace_binding(&f, &execution_id);
    let _broker = f.broker();
    let client = f.client();
    assert!(client.authorize_workspace(&binding).is_err(), "mailbox access does not grant workspace access");
    let mut allowed = grant("a");
    allowed.operations.insert("workspace:observe".into());
    assert!(!allowed.scopes.contains(&rudder_engine::execution::workspace_scope(&binding).unwrap()));
    let token = f.journal.issue_capability(&f.owner, &allowed, 0).unwrap();
    f.a.token = token; project_identity(&f.socket, &f.a).unwrap();
    assert!(client.authorize_workspace(&binding).is_err(), "operation alone does not grant a view");
    allowed.scopes.insert(rudder_engine::execution::workspace_scope(&binding).unwrap());
    f.a.token = f.journal.issue_capability(&f.owner, &allowed, 0).unwrap();
    project_identity(&f.socket, &f.a).unwrap();
    let bound = BrokerClient::connect_workspace(f.socket.clone(), f.socket.with_file_name("identity.json"), &BrokerBinding::from(&f.a), &binding).unwrap();
    assert_eq!(bound.binding(), &BrokerBinding::from(&f.a));
    let mut foreign = binding.clone(); foreign.namespace = uuid::Uuid::new_v4().to_string();
    assert!(bound.authorize_workspace(&foreign).is_err());
    let mut replacement = BrokerBinding::from(&f.a); replacement.run_epoch += 1;
    assert!(BrokerClient::connect_workspace(f.socket.clone(), f.socket.with_file_name("identity.json"), &replacement, &binding).is_err());
    f.journal.observe_run_attempt(&f.owner, "a", &uuid::Uuid::new_v4().to_string(), 1).unwrap();
    assert!(bound.authorize_workspace(&binding).is_err(), "replacement execution fences the old helper");
}

#[test]
fn workspace_authorization_deadline_survives_an_unaccepted_connection_backlog() {
    let f = Fixture::new();
    let (listener, connections) = saturated_unaccepted_backlog(&f.socket);
    let binding = workspace_binding(&f, &uuid::Uuid::new_v4().to_string());
    let started = Instant::now();
    let error = BrokerClient::connect_workspace(f.socket.clone(), f.socket.with_file_name("identity.json"), &BrokerBinding::from(&f.a), &binding)
        .err().expect("a saturated listener must not authorize workspace access");
    assert_eq!(error.downcast_ref::<WireError>().map(|error| error.code.as_str()), Some("unavailable"));
    assert!(started.elapsed() < Duration::from_secs(3), "saturated broker connect exceeded authorization bound");
    drop(connections); drop(listener);
}
fn raw(
    f: &Fixture,
    identity: AgentIdentityFile,
    operation: Operation,
    half_close: bool,
) -> Response {
    let mut stream = UnixStream::connect(&f.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let request = Request {
        format_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        identity,
        operation,
    };
    write_frame(&mut stream, &request, MAX_REQUEST).unwrap();
    if half_close {
        stream.shutdown(std::net::Shutdown::Write).unwrap();
    }
    serde_json::from_slice(
        &read_frame(
            &mut stream,
            MAX_RESPONSE,
            Instant::now() + Duration::from_secs(3),
            || Ok(()),
        )
        .unwrap(),
    )
    .unwrap()
}
fn active(broker: &AgentBroker, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while broker.inner.active.lock().unwrap().len() != count {
        assert!(Instant::now() < deadline, "broker active call count");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_half_closed_socket_is_not_cancellation_but_a_closed_peer_is() {
    let (peer, server) = UnixStream::pair().unwrap();
    peer.shutdown(std::net::Shutdown::Write).unwrap();
    for _ in 0..5 {
        assert!(!disconnected(&server), "the peer can still read a response");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(peer);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !disconnected(&server) {
        assert!(Instant::now() < deadline, "full peer close must cancel a pending call");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn exact_identity_authentication_and_half_closed_requests_work() {
    let f = Fixture::new();
    let broker = f.broker();
    let client = f.client();
    assert!(call(&client, "agents_find", json!({})).unwrap()["agents"].is_array());
    let response = raw(
        &f,
        f.a.clone(),
        Operation::Call {
            tool: "agents_inbox".into(),
            arguments: json!({}),
        },
        true,
    );
    assert!(response.result.is_some());
    for identity in [
        f.b.clone(),
        AgentIdentityFile {
            project_id: "another".into(),
            ..f.a.clone()
        },
        AgentIdentityFile {
            token: "invalid-secret-test".into(),
            ..f.a.clone()
        },
    ] {
        let response = raw(&f, identity, Operation::Authenticate, true);
        assert_eq!(response.error.unwrap().code, "unauthorized");
    }
    assert!(call(&client,"agents_send",json!({"idempotency_key":"impersonate","recipient_run":"b","recipient_epoch":1,"scope":"project","message":"peer text","author":"human"})).is_err());
    broker.close();
}

#[test]
fn rotation_remains_same_attempt_and_revocation_is_checked_on_every_call() {
    let mut f = Fixture::new();
    let _broker = f.broker();
    let client = f.client();
    let old = f.a.clone();
    let next = f.rotate();
    assert!(call(&client, "agents_inbox", json!({})).is_ok());
    assert_eq!(
        raw(&f, old, Operation::Authenticate, true)
            .error
            .unwrap()
            .code,
        "unauthorized"
    );
    f.journal
        .revoke_capability(&f.owner, &next.token, 1)
        .unwrap();
    assert!(call(&client, "agents_inbox", json!({})).is_err());
}

#[test]
fn in_flight_requests_cannot_adopt_newly_rotated_authority() {
    let mut f = Fixture::new();
    let broker = f.broker();
    let client = f.client();
    let waiting = client.clone();
    let handle =
        std::thread::spawn(move || call(&waiting, "agents_await", json!({"timeout_ms":30000})));
    active(&broker, 1);
    let old = f.a.clone();
    let next = f.rotate();
    f.journal
        .revoke_capability(&f.owner, &old.token, 1)
        .unwrap();
    assert!(
        handle.join().unwrap().is_err(),
        "old-token await must not switch to the new file token"
    );
    assert!(call(&client, "agents_inbox", json!({})).is_ok());
    assert_eq!(BrokerBinding::from(&next), BrokerBinding::from(&f.a));
}

#[test]
fn epoch_replacement_is_refused_and_socket_restart_preserves_attempt_binding() {
    let mut f = Fixture::new();
    let broker = f.broker();
    let client = f.client();
    broker.close();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !broker.is_closed() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let _next = f.broker();
    assert!(call(&client, "agents_inbox", json!({})).is_ok());
    f.journal.advance_run_epoch(&f.owner, "a", 1, 1).unwrap();
    assert!(call(&client, "agents_inbox", json!({})).is_err());
    let mut replacement = f.a.clone();
    replacement.run_epoch = 2;
    let path = f.socket.with_file_name("identity.json");
    fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();
    assert!(call(&client, "agents_inbox", json!({})).is_err());
}

#[test]
fn active_call_limit_cancellation_disconnect_and_bounded_frames() {
    let f = Fixture::new();
    let broker = f.broker();
    let client = f.client();
    let mut handles = Vec::new();
    let mut cancels = Vec::new();
    for _ in 0..MAX_CALLS {
        let client = client.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        cancels.push(cancel.clone());
        handles.push(std::thread::spawn(move || {
            client.call_with_cancel("agents_await", json!({"timeout_ms":30000}), &cancel)
        }));
    }
    active(&broker, MAX_CALLS);
    assert!(retryable(
        &call(&client, "agents_inbox", json!({})).unwrap_err()
    ));
    for cancel in cancels {
        cancel.store(true, Ordering::Relaxed);
    }
    for handle in handles {
        assert!(handle.join().unwrap().is_err());
    }
    active(&broker, 0);
    let mut malformed = UnixStream::connect(&f.socket).unwrap();
    malformed
        .write_all(&((MAX_REQUEST + 1) as u32).to_be_bytes())
        .unwrap();
    malformed.shutdown(std::net::Shutdown::Write).unwrap();
    drop(malformed);
    assert!(call(&client, "agents_inbox", json!({})).is_ok());
    let mut dropped = UnixStream::connect(&f.socket).unwrap();
    let request = Request {
        format_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        identity: f.a.clone(),
        operation: Operation::Call {
            tool: "agents_await".into(),
            arguments: json!({"timeout_ms":30000}),
        },
    };
    write_frame(&mut dropped, &request, MAX_REQUEST).unwrap();
    active(&broker, 1);
    drop(dropped);
    active(&broker, 0);
}

#[test]
fn unsafe_projection_entries_and_live_endpoint_takeover_are_rejected() {
    let f = Fixture::new();
    let _broker = f.broker();
    assert!(AgentBroker::spawn(f.config()).is_err());
    let identity = f.socket.with_file_name("identity.json");
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_identity(&identity).is_err());
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(f.socket.with_file_name("peer-secret"), "not projected").unwrap();
    assert!(endpoint_directory(&f.socket).is_err());
    let public = f.root.join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(endpoint_directory(&public.join("broker.sock")).is_err());
    assert_eq!(
        fs::metadata(&public).unwrap().mode() & 0o777,
        0o755,
        "refusal must not chmod an existing directory"
    );
}

#[test]
fn token_rotation_publication_gap_recovers_without_replaying_a_dispatched_call() {
    let mut f = Fixture::new();
    let _broker = f.broker();
    let client = f.client();
    let next = AgentIdentityFile {
        token: f
            .journal
            .issue_capability(&f.owner, &grant("a"), 0)
            .unwrap(),
        ..f.a.clone()
    };
    f.journal
        .revoke_capability(&f.owner, &f.a.token, 1)
        .unwrap();
    let socket = f.socket.clone();
    let publish = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        project_identity(&socket, &next).unwrap();
    });
    assert!(client.pending_messages().is_ok());
    publish.join().unwrap();
}

#[test]
fn inherited_nonblocking_connections_deliver_large_frames_to_active_readers() {
    let f = Fixture::new();
    let broker = f.broker();
    let b_socket = socket_path(&f.root, "p", "b", "attempt-1");
    let b_identity = project_identity(&b_socket, &f.b).unwrap();
    let api = AgentApi::open(f.root.join("journal.sqlite"), &b_identity).unwrap();
    for n in 0..2 {
        api.call("agents_send", json!({"idempotency_key":format!("active-large-{n}"),"recipient_run":"a","recipient_epoch":1,"scope":"project","message":"x".repeat(50_000)})).unwrap();
    }
    let (mut client, server) = UnixStream::pair().unwrap();
    server.set_nonblocking(true).unwrap();
    client.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    client.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
    let inner = broker.inner.clone();
    let worker = std::thread::spawn(move || connection_call(&inner, server));
    write_frame(&mut client, &Request {
        format_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        identity: f.a.clone(),
        operation: Operation::Pending,
    }, MAX_REQUEST).unwrap();
    let response: Response = serde_json::from_slice(&read_frame(
        &mut client, MAX_RESPONSE, Instant::now() + Duration::from_secs(3), || Ok(()),
    ).unwrap()).unwrap();
    assert!(response.error.is_none());
    let messages: Vec<MessageEnvelope> = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(messages.len(), 2);
    worker.join().unwrap();
    let _ = fs::remove_dir_all(b_identity.parent().unwrap());
}

#[test]
fn slow_readers_release_their_connection_budget_and_do_not_acknowledge_messages() {
    let f = Fixture::new();
    let broker = f.broker();
    let b_socket = socket_path(&f.root, "p", "b", "attempt-1");
    let b_identity = project_identity(&b_socket, &f.b).unwrap();
    let api = AgentApi::open(f.root.join("journal.sqlite"), &b_identity).unwrap();
    for n in 0..32 {
        api.call("agents_send",json!({"idempotency_key":format!("large-{n}"),"recipient_run":"a","recipient_epoch":1,"scope":"project","message":"x".repeat(50_000)})).unwrap();
    }
    let mut stalled = UnixStream::connect(&f.socket).unwrap();
    write_frame(
        &mut stalled,
        &Request {
            format_version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            identity: f.a.clone(),
            operation: Operation::Pending,
        },
        MAX_REQUEST,
    )
    .unwrap();
    stalled.shutdown(std::net::Shutdown::Write).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut admitted = false;
    while Instant::now() < deadline {
        let count = broker.inner.count.load(Ordering::Relaxed);
        admitted |= count > 0;
        if admitted && count == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(admitted);
    assert_eq!(
        broker.inner.count.load(Ordering::Relaxed),
        0,
        "slow reader budget must expire"
    );
    let inbox = call(&f.client(), "agents_inbox", json!({"limit":1})).unwrap();
    assert_eq!(inbox["cursor"]["sequence"], 0);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    drop(stalled);
    let _ = fs::remove_dir_all(b_identity.parent().unwrap());
}

#[test]
fn handshake_binding_is_checked_and_interrupted_channel_reads_are_retryable() {
    for wrong_binding in [false, true] {
        let f = Fixture::new();
        let listener = UnixListener::bind(&f.socket).unwrap();
        let mut binding = BrokerBinding::from(&f.a);
        if wrong_binding {
            binding.run_id = "b".into();
        }
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let request: Request = serde_json::from_slice(
                &read_frame(
                    &mut stream,
                    MAX_REQUEST,
                    Instant::now() + Duration::from_secs(2),
                    || Ok(()),
                )
                .unwrap(),
            )
            .unwrap();
            write_frame(
                &mut stream,
                &Response {
                    format_version: 1,
                    id: request.id,
                    result: Some(json!({"binding":binding})),
                    error: None,
                },
                MAX_RESPONSE,
            )
            .unwrap();
            drop(stream);
            if !wrong_binding {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                read_frame(
                    &mut stream,
                    MAX_REQUEST,
                    Instant::now() + Duration::from_secs(2),
                    || Ok(()),
                )
                .unwrap();
                stream.shutdown(std::net::Shutdown::Both).unwrap();
            }
        });
        let client =
            BrokerClient::connect(f.socket.clone(), f.socket.with_file_name("identity.json"));
        if wrong_binding {
            assert!(client.is_err());
        } else {
            assert!(
                retryable(&client.unwrap().pending_messages().unwrap_err()),
                "channel pump must recover after a broker closes a pending response"
            );
        }
        server.join().unwrap();
        fs::remove_file(&f.socket).unwrap();
    }
}

#[test]
#[ignore = "fixture exporter used by agent-broker Node integration tests"]
fn export_broker_fixture() {
    let root =
        PathBuf::from(std::env::var_os("RUDDER_AGENT_BROKER_FIXTURE_DIR").expect("fixture root"));
    let mut f = Fixture::at(root.clone(), true);
    let b_socket = socket_path(&root, "p", "b", "attempt-1");
    let b_file = project_identity(&b_socket, &f.b).unwrap();
    let next = AgentIdentityFile {
        token: f
            .journal
            .issue_capability(&f.owner, &grant("a"), 0)
            .unwrap(),
        ..f.a.clone()
    };
    let next_file = root.join("rotated-a.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&next_file)
        .unwrap();
    serde_json::to_writer(&mut file, &next).unwrap();
    fs::write(root.join("fixture.json"),serde_json::to_vec(&json!({"journal":root.join("journal.sqlite"),"a":f.socket.with_file_name("identity.json"),"b":b_file,"a_socket":f.socket,"b_socket":b_socket,"rotated_a":next_file})).unwrap()).unwrap();
}
