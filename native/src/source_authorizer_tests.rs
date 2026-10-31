use super::*;
use rudder_engine::{
    journal::CapabilityIdentity,
    protocol::{ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RootBinding},
    store::ImportPolicy,
    vfs::{
        exclusion_policy_digest,
        partial_rpc::PROTOCOL as PARTIAL_PROTOCOL,
        rpc::{AttemptContext, ObservationBinding},
    },
};
use std::{
    os::unix::{ffi::OsStrExt, fs::symlink},
    sync::atomic::AtomicU64,
};

struct FixtureIssuerPeer {
    revoked: AtomicBool,
}
impl HostSourceIssuerPeer for FixtureIssuerPeer {
    fn check_current(&self) -> Result<()> {
        ensure!(
            !self.revoked.load(Ordering::Acquire),
            "synthetic issuer retired"
        );
        Ok(())
    }
    fn check_peer(&self, stream: &UnixStream) -> Result<()> {
        self.check_current()?;
        ensure!(
            peer_uid(stream)? == unsafe { libc::geteuid() },
            "fixture peer UID"
        );
        #[cfg(target_os = "linux")]
        {
            let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
            ensure!(
                unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_PEERCRED,
                        (&mut credentials as *mut libc::ucred).cast(),
                        &mut size,
                    )
                } == 0
                    && credentials.pid == std::process::id() as libc::pid_t,
                "fixture is not the actual owned test process"
            );
        }
        #[cfg(target_os = "macos")]
        {
            let mut pid: libc::pid_t = 0;
            let mut size = std::mem::size_of_val(&pid) as libc::socklen_t;
            ensure!(
                unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_LOCAL,
                        libc::LOCAL_PEERPID,
                        (&mut pid as *mut libc::pid_t).cast(),
                        &mut size,
                    )
                } == 0
                    && pid == std::process::id() as libc::pid_t,
                "fixture is not the actual owned test process"
            );
        }
        Ok(())
    }
}
fn fixture_peer() -> Arc<FixtureIssuerPeer> {
    Arc::new(FixtureIssuerPeer {
        revoked: AtomicBool::new(false),
    })
}
fn connect(directory: &Path, binding: PartialBinding) -> Result<SourceAuthorizerClient> {
    SourceAuthorizerClient::connect(directory, binding, fixture_peer())
}
fn connect_with_clock(
    directory: &Path,
    binding: PartialBinding,
    clock: Clock,
) -> Result<SourceAuthorizerClient> {
    SourceAuthorizerClient::with_clock(directory, binding, fixture_peer(), clock)
}

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let parent = Path::new("/tmp").canonicalize().unwrap();
        let path = parent.join(format!(
            "rsa-{}",
            &Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn endpoint(&self) -> PathBuf {
        self.0.join("s")
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
        execution_id: Uuid::new_v4().to_string(),
        operation_id: Uuid::new_v4().to_string(),
        view_id: Uuid::new_v4().to_string(),
    };
    PartialBinding {
        protocol: PARTIAL_PROTOCOL.into(),
        format_version: 1,
        launch_request_digest: "a".repeat(64),
        root: RootBinding {
            scope: CacheScope {
                provider_id: "fixture".into(),
                tenant_id: Uuid::new_v4().to_string(),
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
            namespace: Uuid::new_v4().to_string(),
            profile: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture profile"),
            exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
        },
        attempt,
    }
}
struct Block {
    on: usize,
    started: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
struct Authority {
    calls: AtomicUsize,
    deny: AtomicBool,
    panic_on: AtomicUsize,
    expiry: Mutex<Option<(i64, Option<i64>)>>,
    block: Mutex<Option<Block>>,
    dropped: Arc<AtomicBool>,
}
impl Authority {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            deny: AtomicBool::new(false),
            panic_on: AtomicUsize::new(0),
            expiry: Mutex::new(None),
            block: Mutex::new(None),
            dropped: Arc::new(AtomicBool::new(false)),
        })
    }
    fn block(&self, on: usize) -> BlockControl {
        let (started, receive) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        *self.block.lock().unwrap() = Some(Block {
            on,
            started,
            release: wait,
        });
        BlockControl {
            started: receive,
            release: Some(release),
        }
    }
}
impl Drop for Authority {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}
impl HostSourceAuthority for Authority {
    fn authorize(&self, binding: &PartialBinding) -> Result<HostSourceAuthorization> {
        let n = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        assert_ne!(
            self.panic_on.load(Ordering::Acquire),
            n,
            "synthetic host callback panic"
        );
        let block = {
            let mut block = self.block.lock().unwrap();
            if block.as_ref().is_some_and(|value| value.on == n) {
                block.take()
            } else {
                None
            }
        };
        if let Some(block) = block {
            let _ = block.started.send(());
            let _ = block.release.recv();
        }
        ensure!(
            !self.deny.load(Ordering::Acquire),
            "synthetic revoked host issuer"
        );
        let (validated_at_ms, expires_at_ms) = self.expiry.lock().unwrap().unwrap_or((
            i64::try_from(wall_nanoseconds()? / NS_PER_MS).map_err(|_| fault())?,
            None,
        ));
        Ok(HostSourceAuthorization {
            identity: CapabilityIdentity {
                project_id: binding.attempt.project_id.clone(),
                principal: "synthetic-host-issuer".into(),
                run_id: binding.attempt.run_id.clone(),
                run_epoch: binding.attempt.run_epoch,
                owner_epoch: binding.attempt.owner.epoch,
            },
            validated_at_ms,
            expires_at_ms,
        })
    }
}
struct BlockControl {
    started: mpsc::Receiver<()>,
    release: Option<mpsc::SyncSender<()>>,
}
impl BlockControl {
    fn wait(&self) {
        self.started.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}
impl Drop for BlockControl {
    fn drop(&mut self) {
        self.release();
    }
}
struct Server {
    server: Option<SourceAuthorizerServer>,
    temporary: Temporary,
}
impl Server {
    fn new(
        binding: &PartialBinding,
        authority: Arc<dyn HostSourceAuthority>,
        limits: Limits,
    ) -> Self {
        let temporary = Temporary::new();
        let server =
            SourceAuthorizerServer::bind(&temporary.endpoint(), binding.clone(), authority, limits)
                .unwrap();
        Self {
            server: Some(server),
            temporary,
        }
    }
    fn with_wall_clock(
        binding: &PartialBinding,
        authority: Arc<dyn HostSourceAuthority>,
        clock: Clock,
    ) -> Self {
        let temporary = Temporary::new();
        let server = SourceAuthorizerServer::with_wall_clock(
            &temporary.endpoint(),
            binding.clone(),
            authority,
            Limits::default(),
            clock,
        )
        .unwrap();
        Self {
            server: Some(server),
            temporary,
        }
    }
    fn path(&self) -> &Path {
        self.server.as_ref().unwrap().directory()
    }
    fn close(&mut self) -> SourceAuthorizerSettled {
        self.server
            .as_mut()
            .unwrap()
            .await_closed(Duration::from_secs(2))
            .unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(server) = self.server.as_mut() {
            let _ = server.await_closed(Duration::from_secs(2));
        }
        self.server.take();
    }
}
fn fake_clock(value: u64) -> (Arc<AtomicU64>, Clock) {
    let time = Arc::new(AtomicU64::new(value));
    let read = Arc::clone(&time);
    (
        time,
        Arc::new(move || Ok(read.load(Ordering::Acquire) as u128)),
    )
}
fn request(server: &Server, binding: &PartialBinding) -> Request {
    let file = open_directory(server.path()).unwrap();
    let context = read_context(&file, server.path()).unwrap();
    let body = RequestBody {
        format_version: 1,
        protocol: PROTOCOL.into(),
        purpose: PURPOSE.into(),
        server_instance: context.server_instance,
        request_id: Uuid::new_v4().to_string(),
        serial: 1,
        binding: binding.clone(),
    };
    let mac = sign(&context.credential, REQUEST_DOMAIN, binding, &body).unwrap();
    Request { body, mac }
}
fn raw(server: &Server, bytes: &[u8]) -> Result<Reply> {
    let mut stream = connect_socket(
        &server.path().join(SOCKET),
        Instant::now() + Duration::from_secs(1),
        || Ok(()),
    )?;
    configure(&stream)?;
    let mut frame = Vec::new();
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(bytes);
    io_until(
        &mut stream,
        &mut frame,
        true,
        Instant::now() + Duration::from_secs(1),
        || Ok(()),
    )?;
    receive(
        &mut stream,
        MAX_REPLY,
        Instant::now() + Duration::from_secs(1),
        || Ok(()),
    )
}
fn wait_for(test: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !test() {
        assert!(Instant::now() < deadline);
        thread::sleep(POLL);
    }
}

#[test]
fn real_unix_exchange_checks_actual_callback_and_exact_source_only_binding() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let client = connect(server.path(), binding.clone()).unwrap();
    client.check_current().unwrap();
    assert_eq!(authority.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fs::symlink_metadata(server.path()).unwrap().mode() & 0o7777,
        0o700
    );
    assert_eq!(
        fs::symlink_metadata(server.path().join(CONTEXT))
            .unwrap()
            .mode()
            & 0o7777,
        0o600
    );
    client.authorize(&binding).unwrap();
    assert_eq!(authority.calls.load(Ordering::Acquire), 2);
    let mut other = binding.clone();
    other.launch_request_digest = "b".repeat(64);
    assert!(client.authorize(&other).is_err());
    assert!(client.authorize(&binding).is_err());
    let _settled = server.close();
    assert!(server.temporary.endpoint().join(CONTEXT).exists());
}

#[test]
fn wrong_purpose_token_binding_version_and_duplicate_fields_never_call_host_issuer() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    for change in 0..6 {
        let mut value = request(&server, &binding);
        match change {
            0 => value.body.purpose = "workspace:partial".into(),
            1 => value.body.protocol = "rudder_partial_workspace_v1".into(),
            2 => value.mac = [0; 32],
            3 => value.body.binding.launch_request_digest = "f".repeat(64),
            4 => value.body.format_version = 2,
            _ => value.body.server_instance = Uuid::new_v4().to_string(),
        };
        assert!(raw(&server, &canonical_json(&value).unwrap()).is_err());
    }
    let duplicate_request = request(&server, &binding);
    let bytes = canonical_json(&duplicate_request).unwrap();
    let mut duplicate = b"{\"body\":".to_vec();
    duplicate.extend_from_slice(&canonical_json(&duplicate_request.body).unwrap());
    duplicate.push(b',');
    duplicate.extend_from_slice(&bytes[1..]);
    assert!(raw(&server, &duplicate).is_err());
    assert_eq!(authority.calls.load(Ordering::Acquire), 0);
    let _settled = server.close();
}

#[test]
fn callback_revocation_is_absorbing_and_never_revives_from_later_allow() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let client = connect(server.path(), binding.clone()).unwrap();
    authority.deny.store(true, Ordering::Release);
    assert!(client.authorize(&binding).is_err());
    authority.deny.store(false, Ordering::Release);
    assert!(client.check_current().is_err());
    assert!(client.authorize(&binding).is_err());
    assert!(connect(server.path(), binding.clone()).is_err());
    assert_eq!(authority.calls.load(Ordering::Acquire), 2);
    let _settled = server.close();
}

#[test]
fn actual_reply_cannot_extend_from_arrival_after_sent_time_expiry() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let (time, clock) = fake_clock(100);
    let client = Arc::new(connect_with_clock(server.path(), binding.clone(), clock).unwrap());
    let mut blocked = authority.block(2);
    let calling = Arc::clone(&client);
    let offered = binding.clone();
    let worker = thread::spawn(move || calling.authorize(&offered));
    blocked.wait();
    time.store(3_000_000_100, Ordering::Release);
    blocked.release();
    assert!(worker.join().unwrap().is_err());
    time.store(101, Ordering::Release);
    assert!(client.check_current().is_err());
    assert!(client.authorize(&binding).is_err());
    let _settled = server.close();
}

#[test]
fn real_out_of_order_responses_never_overwrite_newer_sent_time() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let (time, clock) = fake_clock(100);
    let client = Arc::new(connect_with_clock(server.path(), binding.clone(), clock).unwrap());
    let mut blocked = authority.block(2);
    time.store(200, Ordering::Release);
    let older = Arc::clone(&client);
    let offered = binding.clone();
    let first = thread::spawn(move || older.authorize(&offered));
    blocked.wait();
    time.store(300, Ordering::Release);
    client.authorize(&binding).unwrap();
    time.store(400, Ordering::Release);
    blocked.release();
    first.join().unwrap().unwrap();
    let fresh = client.state.fresh.lock().unwrap();
    assert_eq!(fresh.accepted, 3);
    assert_eq!(fresh.until, Some(3_000_000_300));
    drop(fresh);
    let _settled = server.close();
}

#[test]
fn suspend_aware_expiry_clock_regression_and_clock_failure_fence_without_reopen() {
    for regression in [false, true] {
        let binding = binding();
        let authority = Authority::new();
        let mut server = Server::new(&binding, authority.clone(), Limits::default());
        let (time, clock) = fake_clock(100);
        let client = connect_with_clock(server.path(), binding.clone(), clock).unwrap();
        time.store(
            if regression { 99 } else { 3_000_000_100 },
            Ordering::Release,
        );
        assert!(client.check_current().is_err());
        time.store(101, Ordering::Release);
        assert!(client.authorize(&binding).is_err());
        assert_eq!(authority.calls.load(Ordering::Acquire), 1);
        let _settled = server.close();
    }
    let state = ClientState {
        clock: Arc::new(|| Err(fault())),
        issuer: fixture_peer(),
        fenced: AtomicBool::new(false),
        fresh: Mutex::new(Freshness {
            last: None,
            until: None,
            next: 0,
            accepted: 0,
        }),
    };
    assert!(state.begin().is_err());
    assert!(state.fenced.load(Ordering::Acquire));
}

#[test]
fn close_keeps_actual_blocked_callback_stream_and_owner_until_settlement() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(
        &binding,
        authority.clone(),
        Limits {
            workers: 1,
            max_admitted: 1,
            ..Limits::default()
        },
    );
    let mut blocked = authority.block(1);
    let directory = server.path().to_path_buf();
    let offered = binding.clone();
    let client = thread::spawn(move || connect(&directory, offered));
    blocked.wait();
    let callback_lifetime = Arc::downgrade(&authority);
    let dropped = Arc::clone(&authority.dropped);
    drop(authority);
    server.server.as_ref().unwrap().close();
    assert!(client.join().unwrap().is_err());
    assert!(server
        .server
        .as_mut()
        .unwrap()
        .await_closed(Duration::from_millis(20))
        .is_err());
    let state = &server.server.as_ref().unwrap().state;
    assert_eq!(state.calls.load(Ordering::Acquire), 1);
    assert_eq!(state.admitted.load(Ordering::Acquire), 1);
    assert_eq!(state.sockets.lock().unwrap().len(), 1);
    assert!(server.path().join(SOCKET).exists());
    assert!(callback_lifetime.upgrade().is_some());
    assert!(!dropped.load(Ordering::Acquire));
    blocked.release();
    let settled = server.close();
    assert!(server.temporary.endpoint().join(CONTEXT).exists());
    assert_eq!(
        server
            .server
            .as_ref()
            .unwrap()
            .state
            .calls
            .load(Ordering::Acquire),
        0
    );
    server.server.take();
    assert!(!dropped.load(Ordering::Acquire));
    drop(settled);
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn abandoned_owner_closes_ingress_without_false_cleanup_or_losing_blocked_callback() {
    let temporary = Temporary::new();
    let binding = binding();
    let authority = Authority::new();
    let mut blocked = authority.block(1);
    let server = SourceAuthorizerServer::bind(
        &temporary.endpoint(),
        binding.clone(),
        authority.clone(),
        Limits::default(),
    )
    .unwrap();
    let directory = server.directory().to_path_buf();
    let client_path = directory.clone();
    let client = thread::spawn(move || connect(&client_path, binding));
    blocked.wait();
    let retained = Arc::downgrade(&authority);
    drop(authority);
    let start = Instant::now();
    drop(server);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(client.join().unwrap().is_err());
    assert!(retained.upgrade().is_some());
    assert!(directory.join(SOCKET).exists());
    assert!(directory.join(CONTEXT).exists());
    blocked.release();
    wait_for(|| retained.upgrade().is_none());
    // Detached actual completion is not a settlement receipt. Preserve this
    // exact private context for explicit recovery, never adopt/recreate it.
    assert!(directory.join(SOCKET).exists());
    assert!(SourceAuthorizerServer::bind(
        &directory,
        self::binding(),
        Authority::new(),
        Limits::default(),
    )
    .is_err());
}

#[test]
fn bounded_admission_never_replaces_a_callback_that_ignored_timeout() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(
        &binding,
        authority.clone(),
        Limits {
            workers: 1,
            max_admitted: 2,
            ..Limits::default()
        },
    );
    let mut blocked = authority.block(1);
    let first_dir = server.path().to_path_buf();
    let offered = binding.clone();
    let first = thread::spawn(move || connect(&first_dir, offered));
    blocked.wait();
    let second_dir = server.path().to_path_buf();
    let offered = binding.clone();
    let second = thread::spawn(move || connect(&second_dir, offered));
    wait_for(|| {
        server
            .server
            .as_ref()
            .unwrap()
            .state
            .admitted
            .load(Ordering::Acquire)
            == 2
    });
    assert!(connect(server.path(), binding.clone()).is_err());
    assert_eq!(authority.calls.load(Ordering::Acquire), 1);
    server.server.as_ref().unwrap().close();
    assert!(server
        .server
        .as_mut()
        .unwrap()
        .await_closed(Duration::from_millis(20))
        .is_err());
    blocked.release();
    assert!(first.join().unwrap().is_err());
    assert!(second.join().unwrap().is_err());
    let _settled = server.close();
}

#[test]
fn credential_context_fifo_symlink_or_writable_metadata_never_blocks_or_authorizes() {
    for scenario in 0..3 {
        let binding = binding();
        let authority = Authority::new();
        let mut server = Server::new(&binding, authority.clone(), Limits::default());
        let context = server.path().join(CONTEXT);
        let held = server.temporary.0.join("held-context");
        fs::rename(&context, &held).unwrap();
        match scenario {
            0 => {
                let name = std::ffi::CString::new(context.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            1 => symlink(&held, &context).unwrap(),
            _ => {
                fs::copy(&held, &context).unwrap();
                fs::set_permissions(&context, fs::Permissions::from_mode(0o666)).unwrap();
            }
        }
        let started = Instant::now();
        assert!(connect(server.path(), binding.clone()).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(authority.calls.load(Ordering::Acquire), 0);
        fs::remove_file(&context).unwrap();
        fs::rename(&held, &context).unwrap();
        let _settled = server.close();
    }
}

#[test]
fn settlement_preserves_socket_replaced_after_actual_identity_check() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority, Limits::default());
    let path = server.path().to_path_buf();
    // Deterministic check -> replacement -> settlement interleaving. No later
    // check-and-unlink is safe: replacement must survive regardless of timing.
    server
        .server
        .as_ref()
        .unwrap()
        .state
        .directory
        .verify()
        .unwrap();
    server.server.as_ref().unwrap().close();
    let socket = path.join(SOCKET);
    let held = server.temporary.0.join("held-socket");
    fs::rename(&socket, &held).unwrap();
    let replacement = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let original_context = fs::read(path.join(CONTEXT)).unwrap();
    let _settled = server.close();
    assert_eq!(fs::read(path.join(CONTEXT)).unwrap(), original_context);
    assert!(socket.exists());
    let _connection = UnixStream::connect(&socket).unwrap();
    assert!(held.exists());
    drop(replacement);
}

#[test]
fn settlement_preserves_directory_replaced_after_actual_identity_check() {
    let binding = binding();
    let mut server = Server::new(&binding, Authority::new(), Limits::default());
    let path = server.path().to_path_buf();
    server
        .server
        .as_ref()
        .unwrap()
        .state
        .directory
        .verify()
        .unwrap();
    server.server.as_ref().unwrap().close();
    let held = server.temporary.0.join("held-directory");
    fs::rename(&path, &held).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    fs::write(path.join("foreign-protected"), b"must-survive").unwrap();
    let _settled = server.close();
    assert_eq!(
        fs::read(path.join("foreign-protected")).unwrap(),
        b"must-survive"
    );
    assert!(held.join(CONTEXT).exists());
    assert!(held.join(SOCKET).exists());
}

#[test]
fn saturated_unaccepted_unix_backlog_has_a_real_connect_deadline() {
    let temporary = Temporary::new();
    let path = temporary.0.join("blocked.sock");
    let listener = UnixListener::bind(&path).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
    let mut held = Vec::new();
    for _ in 0..16 {
        match connect_socket(&path, Instant::now() + Duration::from_millis(30), || Ok(())) {
            Ok(stream) => held.push(stream),
            Err(_) => break,
        }
    }
    assert!(!held.is_empty());
    let started = Instant::now();
    assert!(connect_socket(&path, Instant::now() + Duration::from_millis(60), || Ok(())).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(held);
    drop(listener);
}

#[test]
fn fresh_short_canonical_context_never_adopts_existing_directory() {
    let temporary = Temporary::new();
    let path = temporary.endpoint();
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    fs::write(path.join("protected"), b"unchanged").unwrap();
    assert!(
        SourceAuthorizerServer::bind(&path, binding(), Authority::new(), Limits::default())
            .is_err()
    );
    assert_eq!(fs::read(path.join("protected")).unwrap(), b"unchanged");
    let overlong = temporary.0.join("x".repeat(104));
    assert!(SourceAuthorizerServer::bind(
        &overlong,
        binding(),
        Authority::new(),
        Limits::default()
    )
    .is_err());
    assert!(!overlong.exists());
}

#[test]
fn oversize_frame_is_rejected_before_body_allocation_or_callback() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let mut stream = connect_socket(
        &server.path().join(SOCKET),
        Instant::now() + Duration::from_secs(1),
        || Ok(()),
    )
    .unwrap();
    configure(&stream).unwrap();
    stream.write_all(&u32::MAX.to_be_bytes()).unwrap();
    let mut one = [0u8; 1];
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                assert!(Instant::now() < deadline);
            }
            other => panic!("unexpected bounded-frame result: {other:?}"),
        }
    }
    assert_eq!(authority.calls.load(Ordering::Acquire), 0);
    let _settled = server.close();
}

#[test]
fn authenticated_negative_reply_cannot_be_changed_into_a_grant() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let context = read_context(&open_directory(server.path()).unwrap(), server.path()).unwrap();
    let request = request(&server, &binding);
    authority.deny.store(true, Ordering::Release);
    let mut denied = raw(&server, &canonical_json(&request).unwrap()).unwrap();
    verify(
        &context.credential,
        REPLY_DOMAIN,
        &binding,
        &denied.body,
        &denied.mac,
    )
    .unwrap();
    assert!(!denied.body.authorized);
    assert_eq!(denied.body.valid_for_ns, 0);
    assert_eq!(denied.body.request_mac, request.mac);
    denied.body.authorized = true;
    denied.body.valid_for_ns = FRESH_NS as u64;
    assert!(verify(
        &context.credential,
        REPLY_DOMAIN,
        &binding,
        &denied.body,
        &denied.mac
    )
    .is_err());
    let _settled = server.close();
}

#[test]
fn replacement_listener_never_receives_secret_or_forges_existing_or_initial_freshness() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let client = connect(server.path(), binding.clone()).unwrap();
    let directory = server.path().to_path_buf();
    let context = read_context(&open_directory(&directory).unwrap(), &directory).unwrap();
    let prior = request(&server, &binding);
    let replay = canonical_json(&raw(&server, &canonical_json(&prior).unwrap()).unwrap()).unwrap();
    let _settled = server.close();
    fs::rename(
        directory.join(SOCKET),
        server.temporary.0.join("retired-socket"),
    )
    .unwrap();
    let replacement = UnixListener::bind(directory.join(SOCKET)).unwrap();
    fs::set_permissions(directory.join(SOCKET), fs::Permissions::from_mode(0o600)).unwrap();
    replacement.set_nonblocking(true).unwrap();
    for initial in [false, true] {
        let listener = replacement.try_clone().unwrap();
        let digest = binding.digest().unwrap();
        let replay = replay.clone();
        // The replacement receives neither the context key nor a trusted
        // callback. Its only available tag is the request-direction MAC.
        let spoof = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        thread::sleep(POLL);
                    }
                    Err(_) => panic!("fixture listener failed"),
                }
            };
            configure(&stream).unwrap();
            let captured: serde_json::Value =
                receive(&mut stream, MAX_FRAME, deadline, || Ok(())).unwrap();
            let request: Request = serde_json::from_value(captured.clone()).unwrap();
            let forged = Reply {
                body: ReplyBody {
                    format_version: 1,
                    protocol: PROTOCOL.into(),
                    purpose: PURPOSE.into(),
                    server_instance: request.body.server_instance,
                    request_id: request.body.request_id,
                    serial: request.body.serial,
                    binding_digest: digest,
                    request_mac: request.mac,
                    authorized: true,
                    valid_for_ns: FRESH_NS as u64,
                },
                mac: request.mac,
            };
            if initial {
                // Even an authentic prior allow cannot initialize a new nonce.
                let old: Reply = serde_json::from_slice(&replay).unwrap();
                send(&mut stream, &old, MAX_REPLY, deadline, || Ok(())).unwrap();
            } else {
                send(&mut stream, &forged, MAX_REPLY, deadline, || Ok(())).unwrap();
            }
            canonical_json(&captured).unwrap()
        });
        if initial {
            assert!(connect(&directory, binding.clone()).is_err());
        } else {
            assert!(client.authorize(&binding).is_err());
            assert!(client.check_current().is_err());
        }
        let captured = String::from_utf8(spoof.join().unwrap()).unwrap();
        assert!(!captured.contains(&context.credential));
        assert!(!captured.contains("credential"));
    }
    assert_eq!(authority.calls.load(Ordering::Acquire), 2);
}

#[test]
fn actual_unix_grant_clamps_nanosecond_capability_remainder_and_none_stays_three_seconds() {
    for expiry in [Some(1), None] {
        let binding = binding();
        let authority = Authority::new();
        *authority.expiry.lock().unwrap() = Some((0, expiry));
        let (_, wall) = fake_clock(999_001);
        let mut server = Server::with_wall_clock(&binding, authority, wall);
        let (time, clock) = fake_clock(100);
        let client = connect_with_clock(server.path(), binding, clock).unwrap();
        let remaining = if expiry.is_some() {
            999
        } else {
            FRESH_NS as u64
        };
        assert_eq!(
            client.state.fresh.lock().unwrap().until,
            Some(100 + u128::from(remaining))
        );
        time.store(100 + remaining - 1, Ordering::Release);
        client.check_current().unwrap();
        time.store(100 + remaining, Ordering::Release);
        assert!(client.check_current().is_err());
        time.store(100, Ordering::Release);
        assert!(client.check_current().is_err());
        let _settled = server.close();
    }
}

#[test]
fn expired_future_or_stale_capability_snapshot_cannot_initialize_freshness() {
    for (wall_ns, validated_ms, expiry) in [
        (1_000_000, 0, Some(1)),
        (0, 1, None),
        (4_000_000_000, 0, None),
        (0, 0, Some(-1)),
    ] {
        let binding = binding();
        let authority = Authority::new();
        *authority.expiry.lock().unwrap() = Some((validated_ms, expiry));
        let (_, wall) = fake_clock(wall_ns);
        let mut server = Server::with_wall_clock(&binding, authority.clone(), wall);
        assert!(connect(server.path(), binding).is_err());
        assert!(server
            .server
            .as_ref()
            .unwrap()
            .state
            .fenced
            .load(Ordering::Acquire));
        assert_eq!(authority.calls.load(Ordering::Acquire), 1);
        let _settled = server.close();
    }
}

#[test]
fn callback_wall_clock_regression_fences_instead_of_rebasing_expiry() {
    let binding = binding();
    let authority = Authority::new();
    *authority.expiry.lock().unwrap() = Some((0, None));
    let ticks = Arc::new(AtomicU64::new(1_000));
    let read = Arc::clone(&ticks);
    let wall: Clock = Arc::new(move || Ok(read.fetch_sub(1, Ordering::AcqRel) as u128));
    let mut server = Server::with_wall_clock(&binding, authority, wall);
    assert!(connect(server.path(), binding).is_err());
    assert!(server
        .server
        .as_ref()
        .unwrap()
        .state
        .fenced
        .load(Ordering::Acquire));
    let _settled = server.close();
}

#[test]
fn one_callback_panic_absorbingly_fences_both_real_workers_and_retains_blocked_work() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let client = Arc::new(connect(server.path(), binding.clone()).unwrap());
    let mut blocked = authority.block(2);
    let pending_client = Arc::clone(&client);
    let offered = binding.clone();
    let pending = thread::spawn(move || pending_client.authorize(&offered));
    blocked.wait();
    authority.panic_on.store(3, Ordering::Release);
    assert!(client.authorize(&binding).is_err());
    assert!(pending.join().unwrap().is_err());
    let state = &server.server.as_ref().unwrap().state;
    assert!(state.fenced.load(Ordering::Acquire));
    assert!(state.stopped.load(Ordering::Acquire));
    wait_for(|| state.calls.load(Ordering::Acquire) == 1);
    assert!(server
        .server
        .as_mut()
        .unwrap()
        .await_closed(Duration::from_millis(20))
        .is_err());
    assert_eq!(authority.calls.load(Ordering::Acquire), 3);
    assert!(connect(server.path(), binding).is_err());
    assert_eq!(authority.calls.load(Ordering::Acquire), 3);
    blocked.release();
    let _settled = server.close();
}

#[test]
fn actual_issuer_custody_retirement_and_explicit_invalidation_are_absorbing() {
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let issuer = fixture_peer();
    let client =
        SourceAuthorizerClient::connect(server.path(), binding.clone(), issuer.clone()).unwrap();
    issuer.revoked.store(true, Ordering::Release);
    assert!(client.check_current().is_err());
    issuer.revoked.store(false, Ordering::Release);
    assert!(client.authorize(&binding).is_err());
    let client = connect(server.path(), binding.clone()).unwrap();
    client.invalidate();
    assert!(client.check_current().is_err());
    assert!(client.authorize(&binding).is_err());
    assert_eq!(authority.calls.load(Ordering::Acquire), 2);
    let _settled = server.close();
}

#[test]
fn worker_exit_resource_drop_stays_owned_until_actual_handle_completion() {
    struct HeldExit {
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Drop for HeldExit {
        fn drop(&mut self) {
            let _ = self.started.send(());
            let _ = self.release.recv();
        }
    }
    let binding = binding();
    let mut server = Server::new(&binding, Authority::new(), Limits::default());
    let (started, observed) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let worker = Worker::spawn("synthetic-held-worker-exit", move || {
        let _held = HeldExit {
            started,
            release: wait,
        };
    })
    .unwrap();
    server.server.as_mut().unwrap().workers.push(worker);
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(server
        .server
        .as_mut()
        .unwrap()
        .await_closed(Duration::from_millis(20))
        .is_err());
    assert!(
        !server
            .server
            .as_ref()
            .unwrap()
            .joiner
            .as_ref()
            .unwrap()
            .workers_joined
    );
    release.send(()).unwrap();
    let _settled = server.close();
}

#[test]
fn real_tls_destructor_timeout_retains_actual_join_owner_and_settles_only_after_release() {
    use std::cell::RefCell;
    struct HeldTls {
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Drop for HeldTls {
        fn drop(&mut self) {
            let _ = self.started.send(());
            let _ = self.release.recv();
        }
    }
    thread_local! {
        static TLS: RefCell<Option<HeldTls>> = const { RefCell::new(None) };
    }
    let binding = binding();
    let authority = Authority::new();
    let mut server = Server::new(&binding, authority.clone(), Limits::default());
    let (started, observed) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let mut release = Some(release);
    let worker = Worker::spawn("synthetic-held-tls-exit", move || {
        TLS.with(|slot| {
            *slot.borrow_mut() = Some(HeldTls {
                started,
                release: wait,
            })
        });
    })
    .unwrap();
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    // The function returned, but REAL thread-local destruction is blocked.
    let function_finished = worker.thread.as_ref().unwrap().is_finished();
    eprintln!("synthetic TLS witness: is_finished_before_release={function_finished}");
    server.server.as_mut().unwrap().workers.push(worker);
    let (done, result) = mpsc::sync_channel(1);
    let observer = thread::spawn(move || {
        let started = Instant::now();
        let outcome = server
            .server
            .as_mut()
            .unwrap()
            .await_closed(Duration::from_millis(20));
        let _ = done.send((server, outcome, started.elapsed()));
    });
    // A test-only observer ensures even the old unbounded join is safely
    // released and the actual threads reaped before the negative assertion.
    let early = result.recv_timeout(Duration::from_millis(250));
    let bounded = early.is_ok();
    if !bounded {
        release.take().unwrap().send(()).unwrap();
    }
    let (mut server, outcome, elapsed) =
        early.unwrap_or_else(|_| result.recv_timeout(Duration::from_secs(2)).unwrap());
    observer.join().unwrap();
    if bounded {
        assert!(outcome.is_err());
        assert!(elapsed < Duration::from_millis(250));
        assert!(
            !server
                .server
                .as_ref()
                .unwrap()
                .joiner
                .as_ref()
                .unwrap()
                .workers_joined
        );
        assert!(server.path().join(CONTEXT).exists());
        release.take().unwrap().send(()).unwrap();
        let _settled = server.close();
    } else {
        // Preserve old-code evidence, then assert only after releasing TLS.
        drop(outcome);
    }
    assert!(
        bounded,
        "await_closed blocked in REAL TLS destruction; is_finished={function_finished}"
    );
    assert_eq!(authority.calls.load(Ordering::Acquire), 0);
}

#[test]
fn request_burst_is_bounded_and_refill_preserves_the_sustained_rate() {
    let start = Instant::now();
    let mut rate = RequestRate::new(start, 16);
    for _ in 0..64 {
        rate.admit(start).unwrap();
    }
    assert!(rate.admit(start).is_err());
    let refill = start + Duration::from_millis(1_000);
    for _ in 0..16 {
        rate.admit(refill).unwrap();
    }
    assert!(rate.admit(refill).is_err());
    let later = refill + Duration::from_secs(100);
    for _ in 0..64 {
        rate.admit(later).unwrap();
    }
    assert!(rate.admit(later).is_err());
    assert!(rate.admit(start).is_err());
}
