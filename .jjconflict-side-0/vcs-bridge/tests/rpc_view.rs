#![cfg(feature = "jj")]
mod common;

use anyhow::{Result, ensure};
use jj_lib::git_backend::GitBackend;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::working_copy::{SnapshotOptions, WorkingCopyFactory};
use jj_lib::workspace::Workspace;
use rudder_engine::content::{self, StagedContentStore};
use rudder_engine::directory::{DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory};
use rudder_engine::protocol::canonical_json;
use rudder_engine::store::LocalStore;
use rudder_engine::vfs::rpc::*;
use rudder_engine::vfs::{VfsManager, VfsView, exclusion_policy_digest};
use rudder_engine::workspace::WorkspaceProfile;
use rudder_vcs_bridge::projection::project_source;
use rudder_vcs_bridge::projection_io::ProjectionBudget;
use rudder_vcs_bridge::rpc_view::*;
use rudder_vcs_bridge::vfs_provider::{self, SourceProjectionPolicy};
use rudder_vcs_bridge::working_copy::*;
use rudder_vcs_bridge::*;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

struct Fence {
    attempt: AttemptBinding,
    observation: ObservationBinding,
    active: AtomicBool,
}
impl vfs_provider::AttemptAuthority for Fence {
    fn validate(&self, binding: &AttemptBinding) -> Result<()> {
        ensure!(
            self.active.load(Ordering::Acquire) && *binding == self.attempt,
            "stale VCS attempt"
        );
        Ok(())
    }
}
impl AttemptAuthority for Fence {
    fn validate(&self, binding: &ObservationBinding) -> Result<()> {
        ensure!(
            self.active.load(Ordering::Acquire) && *binding == self.observation,
            "stale workspace attempt"
        );
        Ok(())
    }
}
fn options() -> SnapshotOptions<'static> {
    SnapshotOptions {
        base_ignores: GitIgnoreFile::empty(),
        progress: None,
        start_tracking_matcher: &EverythingMatcher,
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size: u64::MAX,
    }
}
fn token(metadata: &Path) -> LiveObservation {
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(metadata.join(".jj/working_copy/rudder-state.json")).unwrap(),
    )
    .unwrap();
    serde_json::from_value(state["observation"].clone()).unwrap()
}

struct Fixture {
    _data: tempfile::TempDir,
    endpoint: tempfile::TempDir,
    store: LocalStore,
    view: VfsView,
    repo: Arc<ReadonlyRepo>,
    config: RpcViewConfig,
    fence: Arc<Fence>,
    server: Option<ObservationRpc>,
}
impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let endpoint = tempfile::Builder::new()
            .prefix("rv-")
            .tempdir_in(std::fs::canonicalize("/tmp").unwrap())
            .unwrap();
        fs::set_permissions(endpoint.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = data.path().join("private-jj");
        fs::create_dir(&metadata).unwrap();
        let (_wc, repo) = Workspace::init_internal_git(&common::settings(), &metadata)
            .await
            .unwrap();
        let store = LocalStore::open(data.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let profile = WorkspaceProfile::source_only();
        let policy =
            SourceProjectionPolicy::from_import_policy(&profile.source_exclusions()).unwrap();
        let policy_ref = store.put_blob(&canonical_json(&policy).unwrap()).unwrap();
        let profile_ref = store.put_blob(&canonical_json(&profile).unwrap()).unwrap();
        let content = StagedContentStore::new(&store, &stage.id).unwrap();
        let object =
            content::import_file(&content, &mut b"export const n = 1;\n".as_slice()).unwrap();
        let root = PagedDirectory::new(&LocalDirectoryStore::new(&store, &stage.id))
            .build(
                0o755,
                [DirectoryEntry::new(
                    b"source.ts",
                    EntryNode::File {
                        object,
                        logical_size: 20,
                        mode: 0o644,
                    },
                )
                .unwrap()],
            )
            .unwrap();
        store.publish(&stage.id, "fixture-source", &root).unwrap();
        let empty =
            MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
        let projected = project_source(
            store.clone(),
            &root,
            &empty,
            &policy_ref,
            None,
            None,
            ProjectionBudget::default(),
        )
        .await
        .unwrap();
        let baseline = repo
            .store()
            .get_commit(
                repo.view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        let mut tx = repo.start_transaction();
        let baseline = tx
            .repo_mut()
            .rewrite_commit(&baseline)
            .set_tree(projected.tree)
            .write()
            .await
            .unwrap();
        tx.repo_mut()
            .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), baseline.id().clone())
            .unwrap();
        tx.repo_mut().rebase_descendants().await.unwrap();
        let repo = tx.commit("prepared nonempty RPC fixture").await.unwrap();
        let mut attempt = common::binding();
        attempt.view_id = uuid::Uuid::new_v4().to_string();
        attempt.seed = root.clone();
        attempt.profile = profile_ref.clone();
        attempt.projection_policy = policy_ref;
        attempt.baseline = CommitIdentity::from_jj(&baseline);
        attempt.baseline_operation = JjOperationId(repo.op_id().hex());
        let manager = VfsManager::open(data.path().join("vfs"), store.clone()).unwrap();
        manager.prepare_paged(&root).unwrap();
        let view = manager
            .fork_paged(&attempt.view_id, &root, attempt.owner.clone())
            .unwrap();
        let cursor = view.dirty_cursor().unwrap();
        let observation = ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: uuid::Uuid::new_v4().to_string(),
                operation_id: uuid::Uuid::new_v4().to_string(),
            },
            view_id: attempt.view_id.clone(),
            owner: attempt.owner.clone(),
            namespace: cursor.namespace,
            profile: profile_ref,
            exclusion_policy_digest: exclusion_policy_digest(&profile.source_exclusions()).unwrap(),
        };
        let config = RpcViewConfig {
            format_version: 1,
            attempt: attempt.clone(),
            observation: observation.clone(),
            socket: endpoint.path().join("endpoint/workspace.sock"),
            identity_file: endpoint.path().join("endpoint/identity.json"),
            private_repository: metadata,
            private_git: repo
                .store()
                .backend_impl::<GitBackend>()
                .unwrap()
                .git_repo_path()
                .canonicalize()
                .unwrap(),
            projection_state: data.path().join("private-projections"),
            cas_root: store.root().canonicalize().unwrap(),
            policy,
            budget: ProjectionBudget::default(),
        };
        let fence = Arc::new(Fence {
            attempt,
            observation,
            active: AtomicBool::new(true),
        });
        let mut f = Self {
            _data: data,
            endpoint,
            store,
            view,
            repo,
            config,
            fence,
            server: None,
        };
        f.start();
        f
    }
    fn start(&mut self) {
        let witness = Arc::new(
            DurableWorkingCopyWitness::new(self.config.clone(), common::settings()).unwrap(),
        );
        let server = ObservationRpc::bind(
            self.view.clone(),
            RpcConfig {
                attempt: self.config.observation.attempt.clone(),
                profile: self.config.observation.profile.clone(),
                authority: self.fence.clone(),
                adoption_witness: Some(witness),
                limits: RpcLimits::default(),
            },
            &self.endpoint.path().join("endpoint"),
        )
        .unwrap();
        assert_eq!(server.binding(), &self.config.observation);
        assert_eq!(server.socket_path(), self.config.socket);
        self.server = Some(server);
    }
    fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.await_closed(Duration::from_secs(5)).unwrap();
        }
    }
    fn provider(&self) -> Arc<RpcWorkspaceView> {
        Arc::new(
            RpcWorkspaceView::new(self.config.clone(), self.store.clone(), self.fence.clone())
                .unwrap(),
        )
    }
    async fn bind(&self, provider: Arc<RpcWorkspaceView>) -> LiveObservation {
        let baseline = self
            .config
            .attempt
            .baseline
            .tree
            .to_jj(self.repo.store().clone())
            .unwrap();
        let observed = provider.prepare_baseline(&baseline, None).unwrap();
        let current = observed.token.clone();
        bind_prepared_working_copy(
            &self.config.private_repository,
            &self.config.attempt,
            &common::settings(),
            provider,
            observed,
        )
        .await
        .unwrap();
        current
    }
    fn write(&self, bytes: &[u8]) {
        let file = self.view.lookup(1, b"source.ts").unwrap();
        self.view
            .set_attr(file.ino, None, Some(bytes.len() as u64), None, None)
            .unwrap();
        self.view.write(file.ino, 0, bytes, false).unwrap();
    }
    fn client(&self) -> ObservationClient {
        ObservationClient::new(
            &self.config.socket,
            &self.config.identity_file,
            self.config.observation.clone(),
        )
        .unwrap()
    }
    fn head(&self) -> AdoptionReceipt {
        let Reply::Head(Some(head)) = self.client().call(Operation::Head).unwrap() else {
            panic!("head expected")
        };
        head
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn blocking_proxy_stream(stream: &UnixStream, read_timeout: Duration) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))
}
fn frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut n = [0; 4];
    stream.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    if n > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "proxy fixture frame exceeds bound",
        ));
    }
    let mut out = vec![0; n];
    stream.read_exact(&mut out)?;
    Ok(out)
}
fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "proxy fixture frame exceeds bound",
        ));
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(bytes)
}
struct DropReplyProxy {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    drop_observe: Arc<AtomicBool>,
    drop_adopt: Arc<AtomicBool>,
    drop_execute: Arc<AtomicBool>,
    observes: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}
impl DropReplyProxy {
    fn start(path: PathBuf, target: PathBuf) -> Self {
        Self::with_inherited_nonblocking(path, target, false)
    }
    fn with_inherited_nonblocking(
        path: PathBuf,
        target: PathBuf,
        force_inherited_nonblocking: bool,
    ) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let drop_observe = Arc::new(AtomicBool::new(false));
        let drop_adopt = Arc::new(AtomicBool::new(false));
        let drop_execute = Arc::new(AtomicBool::new(false));
        let observes = Arc::new(AtomicUsize::new(0));
        let (s, o, a, x, c) = (
            stop.clone(),
            drop_observe.clone(),
            drop_adopt.clone(),
            drop_execute.clone(),
            observes.clone(),
        );
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut client, _)) => {
                        let _ = (|| -> Result<()> {
                            if force_inherited_nonblocking {
                                // Exercise inherited Darwin socket flags on
                                // Linux too, with real response backpressure.
                                client.set_nonblocking(true)?;
                                let size: libc::c_int = 4096;
                                let result = unsafe {
                                    libc::setsockopt(
                                        client.as_raw_fd(),
                                        libc::SOL_SOCKET,
                                        libc::SO_SNDBUF,
                                        &size as *const _ as *const libc::c_void,
                                        std::mem::size_of_val(&size) as libc::socklen_t,
                                    )
                                };
                                if result != 0 {
                                    return Err(io::Error::last_os_error().into());
                                }
                            }
                            // BSD accept inherits O_NONBLOCK; this proxy uses
                            // bounded blocking framing, including on replies.
                            blocking_proxy_stream(&client, Duration::from_secs(2))?;
                            let request = frame(&mut client)?;
                            let json: serde_json::Value = serde_json::from_slice(&request)?;
                            let op = json["operation"]["operation"].as_str().unwrap_or("");
                            let mut server = UnixStream::connect(&target)?;
                            // Existing managed commands may execute for 90s.
                            blocking_proxy_stream(&server, Duration::from_secs(100))?;
                            write_frame(&mut server, &request)?;
                            let response = frame(&mut server)?;
                            if op == "observe" {
                                c.fetch_add(1, Ordering::SeqCst);
                                if o.swap(false, Ordering::AcqRel) {
                                    return Ok(());
                                }
                            }
                            if op == "adopt" && a.swap(false, Ordering::AcqRel) {
                                return Ok(());
                            }
                            if op == "execute" && x.swap(false, Ordering::AcqRel) {
                                return Ok(());
                            }
                            write_frame(&mut client, &response)?;
                            Ok(())
                        })();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            path,
            stop,
            drop_observe,
            drop_adopt,
            drop_execute,
            observes,
            thread: Some(thread),
        }
    }
}
impl Drop for DropReplyProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn inherited_nonblocking_proxy_delivers_large_reply_under_backpressure() {
    let endpoint = tempfile::Builder::new()
        .prefix("rv-nb-")
        .tempdir_in(std::fs::canonicalize("/tmp").unwrap())
        .unwrap();
    let target = endpoint.path().join("target.sock");
    let listener = UnixListener::bind(&target).unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = DropReplyProxy::with_inherited_nonblocking(
        endpoint.path().join("proxy.sock"),
        target,
        true,
    );
    let expected = vec![0x73; MAX_RESPONSE_BYTES];
    let response = expected.clone();
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "proxy did not connect");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("proxy target accept: {error}"),
            }
        };
        blocking_proxy_stream(&stream, Duration::from_secs(2)).unwrap();
        assert_eq!(
            frame(&mut stream).unwrap(),
            b"{\"operation\":{\"operation\":\"head\"}}"
        );
        write_frame(&mut stream, &response).unwrap();
        sent.send(()).unwrap();
    });
    let mut client = UnixStream::connect(&proxy.path).unwrap();
    blocking_proxy_stream(&client, Duration::from_secs(5)).unwrap();
    write_frame(&mut client, b"{\"operation\":{\"operation\":\"head\"}}").unwrap();
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    // No response bytes are consumed until the proxy's small send buffer fills.
    thread::sleep(Duration::from_millis(50));
    let actual = frame(&mut client).unwrap();
    assert_eq!(actual.len(), expected.len());
    assert!(actual == expected, "proxy altered the framed response body");
    server.join().unwrap();
}

#[test]
fn real_owning_rpc_projects_nonempty_source_and_adopts_actual_fsynced_wc_without_lock_recursion() {
    pollster::block_on(async {
        let f = Fixture::new().await;
        let provider = f.provider();
        let old = f.bind(provider.clone()).await;
        assert_eq!(f.head().current, old.request_nonce);
        let factory = RudderWorkingCopyFactory::new(provider.clone(), None);
        let wc = factory
            .load_working_copy(
                f.repo.store().clone(),
                f.config.private_repository.clone(),
                f.config.working_copy_state(),
                &common::settings(),
            )
            .unwrap();
        let mut locked = wc.start_mutation().unwrap();
        f.write(b"export const n = 2;\n");
        let (tree, _) = locked.snapshot(&options()).await.unwrap();
        let baseline = f
            .repo
            .store()
            .get_commit(
                f.repo
                    .view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        let mut tx = f.repo.start_transaction();
        let changed = tx
            .repo_mut()
            .rewrite_commit(&baseline)
            .set_tree(tree.clone())
            .write()
            .await
            .unwrap();
        assert_eq!(changed.change_id(), baseline.change_id());
        tx.repo_mut()
            .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), changed.id().clone())
            .unwrap();
        tx.repo_mut().rebase_descendants().await.unwrap();
        let repo = tx.commit("RPC source snapshot").await.unwrap();
        locked.finish(repo.op_id().clone()).await.unwrap();
        let current = token(&f.config.private_repository);
        assert_eq!(f.head().current, current.request_nonce);
        assert_eq!(f.head().sequence, 2);
        assert!(
            !f.store
                .retention()
                .unwrap()
                .pins
                .contains_key(&old.retention)
        );
        assert!(
            f.store
                .retention()
                .unwrap()
                .pins
                .contains_key(&current.retention)
        );
        assert!(provider.work_for(&current).unwrap().cas_content_bytes > 0);
        assert!(!f.config.private_repository.join("source.ts").exists());
        // A request hash/tree supplied without matching private state is data,
        // not an adoption witness; the active root stays retained.
        let nonce = uuid::Uuid::new_v4().to_string();
        let _ = f
            .client()
            .call(Operation::Observe {
                nonce: nonce.clone(),
            })
            .unwrap();
        assert!(
            f.client()
                .call(Operation::Adopt {
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    expected_current: Some(current.request_nonce.clone()),
                    nonce,
                    evidence: AdoptionEvidence {
                        state_sha256: "a".repeat(64),
                        tree: serde_json::to_value(
                            &tree
                                .tree_ids()
                                .iter()
                                .map(|id| id.hex())
                                .collect::<Vec<_>>()
                        )
                        .unwrap()
                    }
                })
                .is_err()
        );
        assert_eq!(f.head().current, current.request_nonce);
    });
}

#[test]
fn lost_capture_reply_and_helper_restart_keep_original_root_until_explicit_safe_retirement() {
    pollster::block_on(async {
        let mut f = Fixture::new().await;
        let initial = f.bind(f.provider()).await;
        let proxy = DropReplyProxy::start(
            f.endpoint.path().join("proxy.sock"),
            f.config.socket.clone(),
        );
        f.config.socket = proxy.path.clone();
        let provider = f.provider();
        f.write(b"export const n = 2;\n");
        proxy.drop_observe.store(true, Ordering::Release);
        let previous = f.config.attempt.baseline.tree.clone();
        assert!(
            provider
                .observe(
                    &f.config.attempt,
                    Some(&initial),
                    &previous,
                    &options(),
                    f.repo.store().clone()
                )
                .await
                .is_err()
        );
        let captured = provider.inspect_pending_capture().unwrap().unwrap();
        let old = captured.observation.as_ref().unwrap().directory.clone();
        assert!(captured.retained);
        let calls = proxy.observes.load(Ordering::SeqCst);
        assert!(
            provider
                .observe(
                    &f.config.attempt,
                    Some(&initial),
                    &previous,
                    &options(),
                    f.repo.store().clone()
                )
                .await
                .is_err()
        );
        assert_eq!(proxy.observes.load(Ordering::SeqCst), calls);
        f.write(b"export const n = 3;\n");
        let proxy_socket = f.config.socket.clone();
        f.config.socket = f.endpoint.path().join("endpoint/workspace.sock");
        f.stop();
        f.start();
        f.config.socket = proxy_socket;
        assert_eq!(
            provider
                .inspect_pending_capture()
                .unwrap()
                .unwrap()
                .observation
                .unwrap()
                .directory,
            old
        );
        provider.retire_uncommitted_capture().unwrap();
        assert!(provider.inspect_pending_capture().unwrap().is_none());
        assert!(
            f.store
                .retention()
                .unwrap()
                .pins
                .contains_key(&initial.retention)
        );
        let next = provider
            .observe(
                &f.config.attempt,
                Some(&initial),
                &previous,
                &options(),
                f.repo.store().clone(),
            )
            .await
            .unwrap();
        assert_ne!(next.token.directory, old);
        assert_ne!(next.token.request_nonce, captured.nonce);
    });
}

#[test]
fn lost_adoption_reply_reconciles_same_operation_after_restart_and_old_attempt_never_adopts_new_binding()
 {
    pollster::block_on(async {
        let mut f = Fixture::new().await;
        let initial = f.bind(f.provider()).await;
        let proxy = DropReplyProxy::start(
            f.endpoint.path().join("proxy.sock"),
            f.config.socket.clone(),
        );
        f.config.socket = proxy.path.clone();
        let provider = f.provider();
        let factory = RudderWorkingCopyFactory::new(provider.clone(), None);
        let wc = factory
            .load_working_copy(
                f.repo.store().clone(),
                f.config.private_repository.clone(),
                f.config.working_copy_state(),
                &common::settings(),
            )
            .unwrap();
        let mut locked = wc.start_mutation().unwrap();
        f.write(b"export const n = 9;\n");
        let (tree, _) = locked.snapshot(&options()).await.unwrap();
        let baseline = f
            .repo
            .store()
            .get_commit(
                f.repo
                    .view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        let mut tx = f.repo.start_transaction();
        let changed = tx
            .repo_mut()
            .rewrite_commit(&baseline)
            .set_tree(tree)
            .write()
            .await
            .unwrap();
        tx.repo_mut()
            .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), changed.id().clone())
            .unwrap();
        tx.repo_mut().rebase_descendants().await.unwrap();
        let repo = tx.commit("RPC adoption lost reply").await.unwrap();
        proxy.drop_adopt.store(true, Ordering::Release);
        assert!(locked.finish(repo.op_id().clone()).await.is_err());
        let current = token(&f.config.private_repository);
        let head = f.head();
        assert_eq!(head.current, current.request_nonce);
        assert_eq!(head.sequence, 2);
        assert!(
            !f.store
                .retention()
                .unwrap()
                .pins
                .contains_key(&initial.retention)
        );
        let observes = proxy.observes.load(Ordering::SeqCst);
        let proxy_socket = f.config.socket.clone();
        f.config.socket = f.endpoint.path().join("endpoint/workspace.sock");
        f.stop();
        f.start();
        f.config.socket = proxy_socket;
        factory
            .load_working_copy(
                repo.store().clone(),
                f.config.private_repository.clone(),
                f.config.working_copy_state(),
                &common::settings(),
            )
            .unwrap();
        assert_eq!(f.head().operation_id, head.operation_id);
        assert_eq!(f.head().sequence, 2);
        assert_eq!(proxy.observes.load(Ordering::SeqCst), observes);
        assert!(
            !f.config
                .projection_state
                .join("adoption-pending.json")
                .exists()
        );
        let before = fs::read(f.config.working_copy_state().join("rudder-state.json")).unwrap();
        f.fence.active.store(false, Ordering::Release);
        assert!(provider.validate_authority(&f.config.attempt).is_err());
        assert!(
            factory
                .load_working_copy(
                    repo.store().clone(),
                    f.config.private_repository.clone(),
                    f.config.working_copy_state(),
                    &common::settings()
                )
                .is_err()
        );
        assert_eq!(
            fs::read(f.config.working_copy_state().join("rudder-state.json")).unwrap(),
            before
        );
    });
}

#[cfg(feature = "cli")]
fn current_repo(config: &RpcViewConfig) -> Arc<ReadonlyRepo> {
    let loader = jj_lib::repo::RepoLoader::init_from_file_system(
        &common::settings(),
        &config.private_repository.join(".jj/repo"),
        &jj_lib::repo::StoreFactories::default(),
    )
    .unwrap();
    let heads = pollster::block_on(loader.op_heads_store().get_op_heads()).unwrap();
    assert_eq!(heads.len(), 1);
    let op = pollster::block_on(loader.load_operation(&heads[0])).unwrap();
    pollster::block_on(loader.load_at(&op)).unwrap()
}
#[cfg(feature = "cli")]
fn service_config(f: &Fixture) -> rudder_vcs_bridge::command_transport::CommandServiceConfig {
    use sha2::{Digest, Sha256};
    // Synthetic upstream init installs sample hooks. Production uses the
    // sanitized metadata seed; model the same empty hook boundary here.
    let hooks = f.config.private_git.join("hooks");
    if hooks.exists() {
        fs::remove_dir_all(hooks).unwrap();
    }
    rudder_vcs_bridge::command_transport::CommandServiceConfig {
        format_version: 1,
        view: f.config.clone(),
        binary: PathBuf::from(env!("CARGO_BIN_EXE_rudder-jj")),
        state_directory: f._data.path().join("command-state"),
        git_config_sha256: hex::encode(Sha256::digest(
            fs::read(f.config.private_git.join("config")).unwrap(),
        )),
        user_name: "Rudder Managed Agent".into(),
        user_email: "managed-agent@invalid.example".into(),
        command_timeout_ms: 90_000,
    }
}

#[cfg(feature = "cli")]
#[test]
fn actual_jj_cli_status_diff_new_and_single_observation_edit_use_scoped_rpc_without_guest_host_argv()
 {
    use rudder_vcs_bridge::command_transport::*;
    pollster::block_on(async {
        let f = Fixture::new().await;
        let provider = f.provider();
        f.bind(provider.clone()).await;
        let attempt = rudder_vcs_bridge::attempt::initialize_attempt_history(
            &f.config.private_repository,
            &f.config.attempt,
            &common::settings(),
            provider,
        )
        .await
        .unwrap();
        assert_ne!(
            attempt.working_copy.change,
            f.config.attempt.baseline.change
        );
        let config = service_config(&f);
        let service =
            CommandService::bind(config.clone(), &f.endpoint.path().join("commands")).unwrap();
        let client = CommandClient::new(service.client_config()).unwrap();
        let status = client
            .execute(
                &uuid::Uuid::new_v4().to_string(),
                SemanticCommand::Status {},
            )
            .unwrap();
        assert_eq!(status.state, CommandState::Completed);
        assert_eq!(
            status.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&hex::decode(&status.stderr_hex).unwrap())
        );
        f.write(b"export const n = 2;\n");
        let diff = client
            .execute(&uuid::Uuid::new_v4().to_string(), SemanticCommand::Diff {})
            .unwrap();
        assert_eq!(
            diff.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&hex::decode(&diff.stderr_hex).unwrap())
        );
        let patch = String::from_utf8(hex::decode(diff.stdout_hex).unwrap()).unwrap();
        assert!(patch.contains("+export const n = 2;"));
        assert!(patch.contains("-export const n = 1;"));
        let repo = current_repo(&f.config);
        let changed = repo
            .store()
            .get_commit(
                repo.view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            CommitIdentity::from_jj(&changed).change,
            attempt.working_copy.change
        );
        let log = client
            .execute(
                &uuid::Uuid::new_v4().to_string(),
                SemanticCommand::Log { limit: 5 },
            )
            .unwrap();
        assert_eq!(
            log.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&hex::decode(&log.stderr_hex).unwrap())
        );
        assert!(
            String::from_utf8(hex::decode(log.stdout_hex).unwrap())
                .unwrap()
                .contains(&changed.id().hex())
        );
        let new_id = uuid::Uuid::new_v4().to_string();
        let new = client
            .execute(
                &new_id,
                SemanticCommand::New {
                    message: "private RPC child".into(),
                },
            )
            .unwrap();
        assert_eq!(
            new.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&hex::decode(&new.stderr_hex).unwrap())
        );
        let repo = current_repo(&f.config);
        let child = repo
            .store()
            .get_commit(
                repo.view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(child.parent_ids(), [changed.id().clone()]);
        assert_ne!(child.change_id(), changed.change_id());
        let retry = client
            .execute(
                &new_id,
                SemanticCommand::New {
                    message: "private RPC child".into(),
                },
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(&retry).unwrap(),
            serde_json::to_value(&new).unwrap()
        );
        assert_eq!(current_repo(&f.config).op_id(), repo.op_id());
        let edit = client
            .execute(
                &uuid::Uuid::new_v4().to_string(),
                SemanticCommand::Edit {
                    commit: GitCommitId(changed.id().hex()),
                },
            )
            .unwrap();
        assert_eq!(
            edit.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&hex::decode(&edit.stderr_hex).unwrap())
        );
        assert_eq!(edit.backend, "jj-lib-edit");
        let edited = current_repo(&f.config);
        assert_eq!(
            edited.view().get_wc_commit_id(WorkspaceName::DEFAULT),
            Some(changed.id())
        );
        let base = edited
            .store()
            .get_commit(&jj_lib::backend::CommitId::new(
                hex::decode(&f.config.attempt.baseline.commit.0).unwrap(),
            ))
            .unwrap();
        assert_eq!(CommitIdentity::from_jj(&base), f.config.attempt.baseline);
        f.write(b"export const n = 3;\n");
        let before = edited.op_id().clone();
        let unsupported = client
            .execute(
                &uuid::Uuid::new_v4().to_string(),
                SemanticCommand::Edit {
                    commit: GitCommitId(changed.id().hex()),
                },
            )
            .unwrap();
        assert_eq!(unsupported.exit_code, Some(1));
        assert!(
            String::from_utf8(hex::decode(unsupported.stderr_hex).unwrap())
                .unwrap()
                .contains("checkout is unsupported")
        );
        assert_eq!(current_repo(&f.config).op_id(), &before);
        // Unsupported guest argv never reaches either RPC or snapshots.
        let guest_config = f._data.path().join("guest-command-config.json");
        fs::write(
            &guest_config,
            canonical_json(&service.client_config()).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&guest_config, fs::Permissions::from_mode(0o600)).unwrap();
        let head = f.head();
        let escape = f._data.path().join("escape");
        for args in [
            vec![
                "--config".into(),
                format!("ui.editor=touch {}", escape.display()),
                "status".into(),
            ],
            vec!["diff".into(), "--tool".into(), "touch".into()],
            vec!["git".into(), "push".into()],
        ] {
            let out = std::process::Command::new(&config.binary)
                .args(args)
                .env_clear()
                .env("RUDDER_JJ_CLIENT_CONFIG", &guest_config)
                .current_dir(f._data.path())
                .output()
                .unwrap();
            assert!(!out.status.success());
            assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported managed jj"));
        }
        assert!(!escape.exists());
        assert_eq!(f.head().sequence, head.sequence);
        assert_eq!(current_repo(&f.config).op_id(), &before);
        let literal = format!("--config=ui.editor='touch {}'", escape.display());
        let safe = client
            .execute(
                &uuid::Uuid::new_v4().to_string(),
                SemanticCommand::New {
                    message: literal.clone(),
                },
            )
            .unwrap();
        assert_eq!(
            safe.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&hex::decode(safe.stderr_hex).unwrap())
        );
        let repo = current_repo(&f.config);
        let commit = repo
            .store()
            .get_commit(
                repo.view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(commit.description(), format!("{literal}\n"));
        assert!(!escape.exists());
        assert!(!f.config.private_repository.join("source.ts").exists());
        service.await_closed(Duration::from_secs(5)).unwrap();
    });
}

#[cfg(feature = "cli")]
#[test]
fn completed_cli_reply_loss_and_service_restart_return_same_receipt_and_stale_attempt_cannot_start()
{
    use rudder_vcs_bridge::command_transport::*;
    pollster::block_on(async {
        let f = Fixture::new().await;
        let provider = f.provider();
        f.bind(provider.clone()).await;
        rudder_vcs_bridge::attempt::initialize_attempt_history(
            &f.config.private_repository,
            &f.config.attempt,
            &common::settings(),
            provider,
        )
        .await
        .unwrap();
        let config = service_config(&f);
        let service =
            CommandService::bind(config.clone(), &f.endpoint.path().join("commands")).unwrap();
        let mut client_config = service.client_config();
        let proxy = DropReplyProxy::start(
            f.endpoint.path().join("cmd-proxy.sock"),
            client_config.socket.clone(),
        );
        client_config.socket = proxy.path.clone();
        let client = CommandClient::new(client_config).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let command = SemanticCommand::New {
            message: "one effect despite lost reply".into(),
        };
        proxy.drop_execute.store(true, Ordering::Release);
        assert!(client.execute(&id, command.clone()).is_err());
        let completed = client.inspect(&id).unwrap();
        assert_eq!(completed.state, CommandState::Completed);
        assert_eq!(
            completed.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&hex::decode(&completed.stderr_hex).unwrap())
        );
        let after = current_repo(&f.config).op_id().clone();
        service.await_closed(Duration::from_secs(5)).unwrap();
        drop(service);
        let service = CommandService::bind(config, &f.endpoint.path().join("commands")).unwrap();
        let retry = client.execute(&id, command).unwrap();
        assert_eq!(
            serde_json::to_value(retry).unwrap(),
            serde_json::to_value(completed).unwrap()
        );
        assert_eq!(current_repo(&f.config).op_id(), &after);
        let before = fs::read(f.config.working_copy_state().join("rudder-state.json")).unwrap();
        f.fence.active.store(false, Ordering::Release);
        assert!(
            client
                .execute(
                    &uuid::Uuid::new_v4().to_string(),
                    SemanticCommand::Status {}
                )
                .is_err()
        );
        assert_eq!(current_repo(&f.config).op_id(), &after);
        assert_eq!(
            fs::read(f.config.working_copy_state().join("rudder-state.json")).unwrap(),
            before
        );
        service.await_closed(Duration::from_secs(5)).unwrap();
    });
}

#[cfg(feature = "cli")]
#[test]
fn trusted_host_wrapper_prepares_nonempty_wc_and_inspects_ready_history_without_reinitializing() {
    use rudder_vcs_bridge::{command_transport::*, host};
    pollster::block_on(async {
        let f = Fixture::new().await;
        let config = service_config(&f);
        let ready = host::prepare_workspace(&config, f.fence.clone()).unwrap();
        assert_ne!(ready.working_copy.change, f.config.attempt.baseline.change);
        let service =
            CommandService::bind(config.clone(), &f.endpoint.path().join("host-commands")).unwrap();
        let client = CommandClient::new(service.client_config()).unwrap();
        f.write(b"export const n = 7;\n");
        assert_eq!(
            client
                .execute(
                    &uuid::Uuid::new_v4().to_string(),
                    SemanticCommand::Status {}
                )
                .unwrap()
                .exit_code,
            Some(0)
        );
        service.await_closed(Duration::from_secs(5)).unwrap();
        let before = current_repo(&f.config).op_id().clone();
        let again = host::prepare_workspace(&config, f.fence.clone()).unwrap();
        assert_eq!(again, ready);
        assert_eq!(current_repo(&f.config).op_id(), &before);
        assert!(!f.config.private_repository.join("source.ts").exists());
    });
}

#[cfg(feature = "cli")]
#[test]
fn command_close_settles_actual_owned_child_as_uncertain_and_holds_canonical_writer_fence() {
    use fs2::FileExt;
    use rudder_vcs_bridge::{command_transport::*, host};
    pollster::block_on(async {
        let f = Fixture::new().await;
        let mut config = service_config(&f);
        host::prepare_workspace(&config, f.fence.clone()).unwrap();
        let marker = f._data.path().join("owned-cli-started");
        let script = f._data.path().join("owned-test-cli.sh");
        // Controlled fixture executable only. The production native factory
        // requires its pinned image ELF and never accepts a shell fallback.
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nsleep 30\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        config.binary = script;
        let service =
            CommandService::bind(config, &f.endpoint.path().join("closing-commands")).unwrap();
        let client = CommandClient::new(service.client_config()).unwrap();
        let request = thread::spawn(move || {
            client.execute(
                &uuid::Uuid::new_v4().to_string(),
                SemanticCommand::Status {},
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "owned fixture CLI failed to start"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let guard = service.quiesce(Duration::from_secs(8)).unwrap();
        let receipt = request.join().unwrap().unwrap();
        assert_eq!(receipt.state, CommandState::Uncertain);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.config.private_repository.join("rudder-command.lock"))
            .unwrap();
        assert!(file.try_lock_exclusive().is_err());
        drop(guard);
        file.try_lock_exclusive().unwrap();
    });
}
