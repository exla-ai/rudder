//! Ignored disposable-VM component qualification. Invoke ONLY the root parent
//! filter. Engine/service remain alive with actual owners until owned VM teardown.
use super::*;
use crate::root_storage_client::{ClientSeedOwner, RootStorageClient};
use jj_lib::{backend::{CommitId, CopyId, TreeValue}, config::{ConfigLayer, ConfigSource, StackedConfig},
    git_backend::GitBackend, merge::Merge, merged_tree::MergedTree,
    merged_tree_builder::MergedTreeBuilder, object_id::ObjectId as _, op_store::RefTarget,
    ref_name::{RefName, WorkspaceName}, repo::{ReadonlyRepo, Repo, RepoLoader, StoreFactories},
    repo_path::RepoPathBuf, settings::UserSettings, workspace::Workspace};
use rudder_engine::{protocol::OwnerEpoch, snapshot::{decode_runnable, decode_source, EnvironmentV2},
    store::DirectoryManifest, workspace::WorkspaceProfile};
use rudder_vcs_bridge::{AttemptBinding, CommitIdentity, GitCommitId, JjOperationId,
    initial_source::{prepare_initial_source, InitialSourceRequest, PreparedInitialSource}, publication::INTEGRATION_BOOKMARK,
    seed::{fork_private_metadata, VerifiedMetadataTemplate}, vfs_provider::SourceProjectionPolicy};
use std::{ffi::CString, io::{self, Write}, os::{unix::{ffi::OsStrExt, process::CommandExt}},
    process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio}, time::Instant};

const CONSTRAINT_ENV: &str = "RUDDER_ROOT_IPC_CONSTRAINT";
const NONCE_ENV: &str = "RUDDER_ROOT_IPC_NONCE";
const SCOPE: &str = "root-ipc-seed-producer-component-only";

struct HeldDir { path: PathBuf, file: File, identity: (u64, u64), mode: u32 }
fn identity(file: &File) -> Result<(u64, u64)> { id(file) }
impl HeldDir {
    fn user(path: &Path, mode: u32) -> Result<Self> {
        ensure!(path.canonicalize()? == path, "fixture user directory redirects");
        let file = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
        let value = Self { path: path.into(), identity: id(&file)?, file, mode }; value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> {
        for m in [self.file.metadata()?, fs::symlink_metadata(&self.path)?] {
            ensure!(m.is_dir() && !m.file_type().is_symlink() && (m.dev(), m.ino()) == self.identity
                && m.uid() == 1500 && m.gid() == 1500 && m.mode() & 0o7777 == self.mode,
                "fixture original user directory changed");
        } Ok(())
    }
}
const PARENT_BYTES: &[u8] = b"real accepted parent bytes\n";
const ACCEPTED_BYTES: &[u8] = b"real accepted child bytes $Format:%H$\n";
const MUTATED_BYTES: &[u8] = b"private first fork changed bytes\n";

fn fixture_settings() -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", "Rudder Real Seed Fixture")?;
    layer.set_value("user.email", "real-seed@invalid.example")?;
    config.add_layer(layer); Ok(UserSettings::from_config(config)?)
}
fn remaining(budget: &FinalizationBudget) -> Result<()> { budget.remaining()?; Ok(()) }
fn write_head(path: &Path, head: &GitCommitId) -> Result<()> {
    head.validate()?;
    let mut file = OpenOptions::new().write(true).truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
    ensure!(file.metadata()?.is_file() && file.metadata()?.nlink() == 1, "fixture HEAD must be its private regular file");
    file.write_all(format!("{}\n", head.0).as_bytes())?; file.sync_all()?; Ok(())
}
async fn changed_tree(repo: &Arc<ReadonlyRepo>, base: MergedTree, bytes: &[u8], budget: &FinalizationBudget) -> Result<MergedTree> {
    remaining(budget)?;
    let path = RepoPathBuf::from_internal_string("tracked")?;
    let id = repo.store().write_file(&path, &mut &bytes[..]).await?;
    remaining(budget)?;
    let mut tree = MergedTreeBuilder::new(base);
    tree.set_or_remove(path, Merge::resolved(Some(TreeValue::File {
        id, executable: true, copy_id: CopyId::placeholder(),
    })));
    let tree = tree.write_tree().await?; remaining(budget)?; Ok(tree)
}

/// Test-owned real source lease: held directory/lock descriptors, Workspace and
/// repo owner remain outside the operations through helper reap and VM teardown.
/// This is no EngineSourceLease or accepted Journal authority claim.
struct SourceOwner {
    _workspace: Workspace, repository: Arc<ReadonlyRepo>, directory: HeldDir, git: HeldDir,
    lock: File, lock_path: PathBuf, lock_id: (u64, u64), head: CommitIdentity,
}
impl SourceOwner {
    fn check(&self) -> Result<()> {
        self.directory.check()?; self.git.check()?;
        let named = fs::symlink_metadata(&self.lock_path)?; let held = self.lock.metadata()?;
        for m in [&named, &held] {
            ensure!(m.is_file() && !m.file_type().is_symlink() && (m.dev(), m.ino()) == self.lock_id
                && m.uid() == 1500 && m.gid() == 1500 && m.nlink() == 1 && m.mode() & 0o7777 == 0o600,
                "real source fixture lock custody changed");
        }
        ensure!(fs::read(self.git.path.join("HEAD"))? == format!("{}\n", self.head.commit.0).as_bytes(),
            "real source fixture accepted HEAD changed");
        let backend = self.repository.store().backend_impl::<GitBackend>().context("fixture Git backend absent")?;
        ensure!(backend.git_repo().head()?.id().context("fixture Git HEAD unborn")?.detach().to_string() == self.head.commit.0,
            "real source fixture Git HEAD differs"); Ok(())
    }
}
async fn make_source(work: &Path, settings: &UserSettings, budget: &FinalizationBudget) -> Result<SourceOwner> {
    remaining(budget)?;
    let path = work.join("original-source"); fs::DirBuilder::new().mode(0o700).create(&path)?;
    let (workspace, repo) = Workspace::init_internal_git(settings, &path).await?;
    remaining(budget)?;
    let empty = MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone());
    let parent_tree = changed_tree(&repo, empty, PARENT_BYTES, budget).await?;
    let mut tx = repo.start_transaction();
    let parent = tx.repo_mut().new_commit(vec![repo.store().root_commit_id().clone()], parent_tree)
        .set_description("real original fixture parent").clear_sign_key().write().await?;
    remaining(budget)?;
    let tree = changed_tree(&repo, parent.tree(), ACCEPTED_BYTES, budget).await?;
    let head = tx.repo_mut().new_commit(vec![parent.id().clone()], tree)
        .set_description("real original accepted fixture child").clear_sign_key().write().await?;
    tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK), RefTarget::normal(head.id().clone()));
    tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(), head.id().clone())?;
    let repo = tx.commit("publish original fixture history").await?;
    let baseline = CommitIdentity::from_jj(&head);
    let git_path = repo.store().backend_impl::<GitBackend>().context("fixture source Git absent")?.git_repo_path().to_path_buf();
    write_head(&git_path.join("HEAD"), &baseline.commit)?;
    let lock_path = work.join("original-source.lock");
    let lock = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&lock_path)?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let source = SourceOwner { _workspace: workspace, repository: repo,
        directory: HeldDir::user(&path, 0o700)?,
        git: HeldDir::user(&git_path, fs::symlink_metadata(&git_path)?.mode() & 0o7777)?,
        lock_id: identity(&lock)?, lock, lock_path, head: baseline };
    source.check()?; remaining(budget)?; Ok(source)
}
async fn reopen(path: &Path, baseline: &CommitIdentity, operation: &JjOperationId, tracked: &[u8], parent_tracked: &[u8],
    budget: &FinalizationBudget) -> Result<Arc<ReadonlyRepo>> {
    remaining(budget)?; baseline.validate()?; operation.validate()?;
    let loader = RepoLoader::init_from_file_system(&fixture_settings()?, &path.join(".jj/repo"), &StoreFactories::default())?;
    let heads = loader.op_heads_store().get_op_heads().await?;
    ensure!(heads.len() == 1 && heads[0].hex() == operation.0, "real fork operation head differs");
    let repo = loader.load_at_head().await?;
    ensure!(repo.op_id().hex() == operation.0, "reopened real fork operation differs");
    let id = repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).context("real fork WC commit absent")?;
    let commit = repo.store().get_commit(id)?;
    ensure!(CommitIdentity::from_jj(&commit) == *baseline
        && repo.view().get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK)) == &RefTarget::normal(id.clone()),
        "real fork baseline/change/tree/bookmark differs");
    ensure!(baseline.parents.len() == 1, "real fork must preserve one actual parent");
    let parent_id = CommitId::try_from_hex(&baseline.parents[0].0).context("real parent ID invalid")?;
    let parent = repo.store().get_commit(&parent_id)?;
    ensure!(parent.id() == &parent_id, "real accepted parent missing");
    let backend = repo.store().backend_impl::<GitBackend>().context("real fork Git backend absent")?;
    let git = backend.git_repo();
    ensure!(git.head()?.id().context("real fork Git HEAD unborn")?.detach().to_string() == baseline.commit.0,
        "real fork Git HEAD differs from accepted baseline");
    // Pinned jj enables the raw object APIs, without gix's revision parser.
    let git_head = git.head()?.id().context("real fork Git HEAD absent")?.object()?.try_into_commit()?;
    let git_parent = git_head.parent_ids().next().context("real fork Git parent absent")?.object()?.try_into_commit()?;
    let accepted_blob = git_head.tree()?.lookup_entry_by_path("tracked")?.context("accepted Git path absent")?.object()?.try_into_blob()?.take_data();
    let parent_blob = git_parent.tree()?.lookup_entry_by_path("tracked")?.context("parent Git path absent")?.object()?.try_into_blob()?.take_data();
    ensure!(accepted_blob == tracked && parent_blob == parent_tracked, "real fork Git history preimages differ");
    remaining(budget)?; Ok(repo)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateFrame { version: u32, serial: u64, label: String, nonce: String, constraint: FinalizationConstraint }
struct Gate { endpoint: Endpoint, peer: Process, uid: u32, nonce: String, budget: FinalizationBudget, sent: u64, received: u64 }
impl Gate {
    fn capture(endpoint: OwnedFd, original_peer: OwnedFd, uid: u32, nonce: &str, budget: &FinalizationBudget) -> Result<Self> {
        Ok(Self { endpoint: Endpoint::capture(endpoint)?, peer: Process::capture(original_peer)?, uid,
            nonce: nonce.into(), budget: budget.clone(), sent: 0, received: 0 })
    }
    fn send(&mut self, label: &str, body: serde_json::Value, fd: Option<&OwnedFd>) -> Result<()> {
        self.peer.live()?; remaining(&self.budget)?;
        self.sent = self.sent.checked_add(1).context("fixture gate serial exhausted")?;
        let bytes = encode(&GateFrame { version: 1, serial: self.sent, label: label.into(), nonce: self.nonce.clone(),
            constraint: self.budget.constraint() }, &canonical_json(&body)?)?;
        loop { self.peer.live()?; remaining(&self.budget)?;
            if self.endpoint.send(&bytes, fd)? { return Ok(()); }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn receive(&mut self) -> Result<Option<(String, serde_json::Value, Option<OwnedFd>)>> {
        self.peer.live()?; remaining(&self.budget)?;
        let Some(mut packet) = self.endpoint.receive()? else { return Ok(None); };
        ensure!(packet.credentials.pid == self.peer.pid as i32 && packet.credentials.uid == self.uid
            && packet.credentials.gid == self.uid, "fixture gate original peer credentials differ");
        let (header, body): (GateFrame, _) = decode(&packet.bytes)?;
        ensure!(header.version == 1 && header.serial == self.received + 1 && header.nonce == self.nonce
            && header.constraint == self.budget.constraint() && header.label.len() <= 64
            && header.label.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'), "fixture gate binding/order differs");
        self.received = header.serial;
        let value = serde_json::from_slice(body)?; self.peer.live()?; remaining(&self.budget)?;
        Ok(Some((header.label, value, packet.fds.pop())))
    }
    fn expect(&mut self, expected: &str) -> Result<(serde_json::Value, Option<OwnedFd>)> {
        loop { if let Some((label, value, fd)) = self.receive()? {
            ensure!(label == expected, "fixture gate role failed or stage differs"); return Ok((value, fd));
        } std::thread::sleep(Duration::from_millis(2)); }
    }
}
fn take_inherited(fd: i32) -> Result<OwnedFd> {
    ensure!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0, "fixture original inherited fd absent");
    let owned = unsafe { OwnedFd::from_raw_fd(fd) }; fd_cloexec(&owned)?; Ok(owned)
}
fn role_context(uid: u32) -> Result<(String, FinalizationBudget)> {
    ensure!(unsafe { libc::getuid() } == uid && unsafe { libc::geteuid() } == uid
        && unsafe { libc::getgid() } == uid && unsafe { libc::getegid() } == uid, "fixture role actual UID/GID differs");
    floor(std::process::id(), uid)?;
    let mut keys: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect(); keys.sort();
    let mut expected = vec![std::ffi::OsString::from(CONSTRAINT_ENV), std::ffi::OsString::from(NONCE_ENV)]; expected.sort();
    ensure!(keys == expected, "fixture role environment was not cleared");
    let nonce = std::env::var(NONCE_ENV)?; let parsed = uuid::Uuid::parse_str(&nonce)?;
    ensure!(parsed.get_version_num() == 4 && parsed.to_string() == nonce, "fixture nonce invalid");
    let budget = FinalizationBudget::from_constraint(&FinalizationConstraint::from_json(std::env::var(CONSTRAINT_ENV)?.as_bytes())?)?;
    remaining(&budget)?; Ok((nonce, budget))
}
fn role_filter(name: &str) -> String { format!("{}::{name}", module_path!().split_once("::").unwrap().1) }
fn original_child_pidfd(child: &Child) -> Result<Process> {
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
    ensure!(raw >= 0, "fixture original child pidfd unavailable"); Process::capture(unsafe { OwnedFd::from_raw_fd(raw) })
}
struct ChildOwner {
    child: Child, process: Option<Process>, out: Option<ChildStdout>, err: Option<ChildStderr>,
    out_eof: bool, err_eof: bool, out_bytes: usize, err_bytes: usize, status: Option<ExitStatus>,
}
fn drain_pipe(pipe: &mut impl Read, eof: &mut bool, bytes: &mut usize) -> Result<()> {
    if *eof { return Ok(()); }
    let mut buffer = [0u8; 4096];
    for _ in 0..16 {
        match pipe.read(&mut buffer) {
            Ok(0) => { *eof = true; return Ok(()); }
            Ok(n) => { *bytes += n; ensure!(*bytes <= 65536, "fixture child pipe exceeds bound"); }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    } Ok(())
}
impl ChildOwner {
    fn original(&self) -> Result<&Process> { self.process.as_ref().context("fixture original child pidfd not captured") }
    fn poll_exit_and_pipes(&mut self) -> Result<bool> {
        if let Some(out) = &mut self.out { drain_pipe(out, &mut self.out_eof, &mut self.out_bytes)?; }
        if let Some(err) = &mut self.err { drain_pipe(err, &mut self.err_eof, &mut self.err_bytes)?; }
        if self.status.is_none() { self.status = self.child.try_wait()?; }
        if let Some(status) = self.status {
            if self.out_eof && self.err_eof {
                ensure!(status.success() && self.original()?.exited()?, "fixture original child failed or exit uncertain");
                return Ok(true);
            }
        } Ok(false)
    }
    fn await_exit_and_pipes(&mut self, original: &FinalizationBudget) -> Result<()> {
        loop { remaining(original)?; if self.poll_exit_and_pipes()? { remaining(original)?; return Ok(()); }
            std::thread::sleep(Duration::from_millis(2)); }
    }
}
fn start_retaining(children: &mut Vec<ChildOwner>, command: &mut Command) -> Result<usize> {
    let mut child = command.spawn()?; let out = child.stdout.take(); let err = child.stderr.take();
    children.push(ChildOwner { child, process: None, out_eof: out.is_none(), err_eof: err.is_none(), out, err,
        out_bytes: 0, err_bytes: 0, status: None });
    let index = children.len() - 1; let owner = &mut children[index];
    owner.process = Some(original_child_pidfd(&owner.child)?);
    for raw in owner.out.as_ref().map(AsRawFd::as_raw_fd).into_iter().chain(owner.err.as_ref().map(AsRawFd::as_raw_fd)) {
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
        ensure!(flags >= 0 && unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0, "fixture child pipe nonblocking unavailable");
    } Ok(index)
}
fn role_command(name: &str, drop_to_uid1500: bool, captured_pipes: bool, cwd: &Path, nonce: &str,
    budget: &FinalizationBudget, descriptors: &[(&OwnedFd, i32)]) -> Result<(Command, Vec<OwnedFd>)> {
    let mut command = if drop_to_uid1500 {
        let mut c = Command::new("/usr/bin/setpriv");
        c.args(["--reuid=1500", "--regid=1500", "--clear-groups", "--inh-caps=-all", "--ambient-caps=-all", "--bounding-set=-all", "--no-new-privs", "--", ENGINE_IMAGE]); c
    } else { Command::new(ENGINE_IMAGE) };
    command.args(["--exact", &role_filter(name), "--ignored", "--nocapture", "--test-threads=1"])
        .env_clear().env(CONSTRAINT_ENV, String::from_utf8(canonical_json(&budget.constraint())?)?)
        .env(NONCE_ENV, nonce).current_dir(cwd).stdin(Stdio::null());
    if captured_pipes { command.stdout(Stdio::piped()).stderr(Stdio::piped()); }
    else { command.stdout(Stdio::null()).stderr(Stdio::null()); }
    let mut held = Vec::new(); let mut mapping = Vec::new(); let mut targets = BTreeSet::new();
    for (original, target) in descriptors {
        ensure!((3..=5).contains(target) && targets.insert(*target), "fixture fixed inherited fd map invalid");
        let raw = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 20) };
        ensure!(raw >= 20, "fixture original bootstrap fd duplication failed");
        let owner = unsafe { OwnedFd::from_raw_fd(raw) }; mapping.push((raw, *target)); held.push(owner);
    }
    unsafe { command.pre_exec(move || {
        for &(original, target) in &mapping { if libc::dup2(original, target) < 0 { return Err(io::Error::last_os_error()); } }
        Ok(())
    }); }
    Ok((command, held))
}
fn no_fd(fd: Option<OwnedFd>) -> Result<()> { ensure!(fd.is_none(), "fixture unexpected descriptor"); Ok(()) }
fn value_ref(body: &serde_json::Value, key: &str) -> Result<ObjectRef> { Ok(serde_json::from_value(body[key].clone())?) }
fn erofs_head(root: &Path) -> Result<()> {
    let path = root.join("metadata/.git/HEAD"); let m = fs::symlink_metadata(&path)?;
    ensure!(m.is_file() && m.uid() == 1500 && m.gid() == 1500 && m.mode() & 0o7777 == 0o600, "fixture real HEAD ownership/mode differs");
    let before = fs::read(&path)?;
    let attempt = OpenOptions::new().append(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path);
    ensure!(attempt.err().and_then(|e| e.raw_os_error()) == Some(libc::EROFS) && fs::read(path)? == before, "fixture real HEAD did not enforce EROFS"); Ok(())
}
fn retained_forever<T>(owner: &T) -> ! {
    loop { std::hint::black_box(owner); std::thread::sleep(Duration::from_secs(1)); }
}

fn service_snapshot(service: &RootStorageService) -> Result<serde_json::Value> {
    service.current()?;
    let mut slots = Vec::new();
    for (key, slot) in &service.slots {
        let store = slot.store()?; let m = fs::symlink_metadata(store.root())?; let retention = store.retention()?; let pins = &retention.pins;
        let mut helpers = Vec::new();
        for (reader, helper) in &slot.helpers { helpers.push(serde_json::json!({"reader":reader.to_string(),"pid":helper.pid,"exited":helper.exited()?})); }
        let (root, read_checked, baseline, operation) = match &slot.reader {
            Some(reader) => { reader.check_current()?; erofs_head(reader.root())?; let manifest=reader.template()?.manifest();
                (Some(reader.root().to_path_buf()),true,Some(manifest.baseline.clone()),Some(manifest.operation.clone())) }
            None => (None,false,None,None),
        };
        slots.push(serde_json::json!({"slot":key.to_string(),"seed":slot.seed,"published":slot.published,"fenced":slot.fenced,
            "root":root,"baseline":baseline,"operation":operation,"read_owner_retained":slot.reader.is_some(),"read_checked":read_checked,
            "mount_owner_retained":slot.mount.is_some(),"mount_settled":slot.mount.as_ref().is_some_and(|m| m.is_settled()),
            "root_cas_uid":m.uid(),"root_cas_gid":m.gid(),"root_cas_mode":m.mode() & 0o7777,
            "root_cas_identity":[m.dev(),m.ino()],"root_cas":store.root(),"import_pin_retained":pins.get(&slot.import_pin)==Some(&slot.seed),
            "mount_pin_retained":pins.iter().any(|(name,r)| name.starts_with("root-seed-mount:") && r == &slot.seed),
            "stage_owner_retained":slot.stage.as_ref().is_some_and(|id|retention.staging.contains_key(id)),
            "cancel_event_retained":fs::read_link(format!("/proc/self/fd/{}",slot.cancel.event.as_raw_fd()))? == Path::new("anon_inode:[eventfd]"),
            "cancel_flag":slot.cancel.flag.load(Ordering::Acquire),"original_budget_cancelled":slot.budget.remaining().is_err(),
            "cancel_observer_handle_retained":slot.cancel.worker.is_some(),
            "success_observer_actually_joined":slot.cancel.worker.is_none() && slot.cancel.result.as_ref().is_some_and(|r|r.is_ok()),
            "helpers":helpers}));
    }
    Ok(serde_json::json!({"service_pid":service.own.pid,"engine_pid":service.engine.pid,"root_caps":service.root_caps,
        "service_image_sha256":service.service_image.sha256,"engine_image_sha256":service.engine_image.sha256,
        "original_host_namespace_checks":true,"slots":slots,"normal_retirement_blocked":true,"normal_cleanup_performed":false}))
}
struct ServiceRoleOwner { gate: Gate, service: Option<RootStorageService> }

#[test]
#[ignore = "Only paired disposable root IPC parent may invoke this retained root role"]
fn root_storage_ipc_service_role() -> Result<()> {
    let (nonce, original) = role_context(0)?;
    let gate = Gate::capture(take_inherited(3)?, take_inherited(4)?, 0, &nonce, &original)?;
    let mut owner = ServiceRoleOwner { gate, service: None };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let (_, endpoint) = owner.gate.expect("service_endpoint")?;
        let (_, engine) = owner.gate.expect("engine_pidfd")?;
        owner.service = Some(RootStorageService::from_bootstrap(endpoint.context("fixture service endpoint absent")?, engine.context("fixture original engine pidfd absent")?)?);
        owner.gate.send("service_ready", serde_json::json!({"pid":std::process::id()}), None)?;
        let mut paused = false; let mut pause_sent = false;
        loop {
            remaining(&original)?;
            if !paused { owner.service.as_mut().unwrap().poll_once()?; }
            if owner.service.as_ref().unwrap().slots.len() == 2 && !pause_sent {
                paused = true; pause_sent = true;
                owner.gate.send("second_begin_paused", serde_json::json!({}), None)?;
            }
            if let Some((label, _, fd)) = owner.gate.receive()? {
                no_fd(fd)?;
                match label.as_str() {
                    "snapshot" => owner.gate.send("snapshot", service_snapshot(owner.service.as_ref().unwrap())?, None)?,
                    "resume_cancelled" => {
                        ensure!(paused, "fixture second transfer was not paused");
                        loop {
                            remaining(&original)?;
                            let failed = owner.service.as_ref().unwrap().slots.values().find(|s| !s.published).context("fixture cancelled original slot absent")?;
                            if failed.cancel.flag.load(Ordering::Acquire) && failed.budget.remaining().is_err() { break; }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        paused = false; owner.gate.send("cancel_observed", serde_json::json!({}), None)?;
                    }
                    _ => anyhow::bail!("fixture root supervisor request invalid"),
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })).unwrap_or_else(|_|Err(anyhow::anyhow!("fixture root role panicked; actual owners retained")));
    if let Some(service) = &mut owner.service { service.close_admission(); }
    let _ = owner.gate.send("role_failed", serde_json::json!({"owners_retained":owner.service.is_some(),"failed":result.is_err()}), None);
    // This process deliberately survives the root test command. No missing host
    // drain is replaced by killing this owner, Drop or a fabricated capability.
    retained_forever(&owner)
}

fn snapshot(gate: &mut Gate) -> Result<serde_json::Value> {
    gate.send("snapshot", serde_json::json!({}), None)?;
    let (value, fd) = gate.expect("snapshot")?; no_fd(fd)?; Ok(value)
}
fn published_slot(snapshot: &serde_json::Value) -> Result<&serde_json::Value> {
    let slots = snapshot["slots"].as_array().context("fixture root slots absent")?;
    ensure!(slots.iter().filter(|s| s["published"] == true).count() == 1, "fixture root published mount count differs");
    slots.iter().find(|s| s["published"] == true).context("fixture root published slot absent")
}
fn check_helpers(snapshot: &serde_json::Value, expected_exits: usize) -> Result<Vec<u32>> {
    let slot = published_slot(snapshot)?;
    ensure!(slot["read_owner_retained"] == true && slot["read_checked"] == true && slot["mount_owner_retained"] == true
        && slot["mount_settled"] == false && slot["import_pin_retained"] == true && slot["mount_pin_retained"] == true
        && slot["root_cas_uid"] == 0 && slot["root_cas_gid"] == 0 && slot["root_cas_mode"] == 0o700
        && slot["success_observer_actually_joined"] == true, "fixture original root mount/read/import ownership differs");
    let helpers = slot["helpers"].as_array().context("fixture original helper set absent")?;
    ensure!(helpers.len() == 2 && helpers.iter().filter(|h| h["exited"] == true).count() == expected_exits,
        "fixture physical original helper exit set differs");
    let pids = helpers.iter().map(|h| h["pid"].as_u64().and_then(|p|u32::try_from(p).ok()).context("fixture helper pid invalid")).collect::<Result<Vec<_>>>()?;
    ensure!(pids.iter().copied().collect::<BTreeSet<_>>().len() == 2, "fixture enrolled original helpers not distinct"); Ok(pids)
}

struct ParentRoleOwner { children: Vec<ChildOwner>, engine_gate: Option<Gate>, service_gate: Option<Gate>, work: PathBuf }

#[test]
#[ignore = "Root-only IPC component fixture; exact filter on pinned disposable VM, then destroy owned VM"]
fn root_storage_ipc_kernel_qualification() -> Result<()> {
    let original = FinalizationBudget::begin(Duration::from_secs(30))?;
    ensure!(unsafe { libc::getuid() } == 0 && unsafe { libc::geteuid() } == 0
        && unsafe { libc::getgid() } == 0 && unsafe { libc::getegid() } == 0, "IPC parent requires actual root");
    let image = Image::capture(Path::new(ENGINE_IMAGE))?; let own = Process::current()?; image.matches_process(&own)?;
    ensure!(std::env::current_exe()?.canonicalize()? == Path::new(ENGINE_IMAGE), "IPC parent is not fixed pinned ELF");
    let nonce = uuid::Uuid::new_v4().to_string(); let work = Path::new("/run").join(format!("rudder-root-ipc-{nonce}"));
    fs::DirBuilder::new().mode(0o711).create(&work)?;
    let scratch = work.join("engine"); fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    let path = CString::new(scratch.as_os_str().as_bytes())?;
    ensure!(unsafe { libc::chown(path.as_ptr(), 1500, 1500) } == 0, "fixture chown only fresh engine scratch failed");
    let mut owner = Box::new(ParentRoleOwner { children: Vec::new(), engine_gate: None, service_gate: None, work });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<serde_json::Value> {
        let (service_endpoint, engine_endpoint) = bootstrap_socket_pair()?;
        let (engine_parent_gate, engine_child_gate) = bootstrap_socket_pair()?;
        let (mut command, inherited) = role_command("root_storage_ipc_engine_role", true, false, &scratch, &nonce, &original,
            &[(&engine_endpoint,3),(&engine_child_gate,4),(own.fd(),5)])?;
        let engine_index = start_retaining(&mut owner.children, &mut command)?; drop(inherited); drop(engine_endpoint); drop(engine_child_gate);
        let engine_pidfd = owner.children[engine_index].original()?.fd().try_clone()?;
        owner.engine_gate = Some(Gate::capture(engine_parent_gate, engine_pidfd, 1500, &nonce, &original)?);
        let (_, fd) = owner.engine_gate.as_mut().unwrap().expect("engine_waiting")?; no_fd(fd)?;
        let engine = owner.children[engine_index].original()?; image.matches_process(engine)?; floor(engine.pid, 1500)?;
        for (name, kind) in [("mnt",libc::CLONE_NEWNS),("user",libc::CLONE_NEWUSER)] {
            ensure!(id(&namespace(None,name,kind)?)? == id(&namespace(Some(engine.pid),name,kind)?)?, "fixture early engine host namespace differs");
        }
        let (service_parent_gate, service_child_gate) = bootstrap_socket_pair()?;
        let (mut command, inherited) = role_command("root_storage_ipc_service_role", false, false, &owner.work, &nonce, &original,
            &[(&service_child_gate,3),(own.fd(),4)])?;
        let service_index = start_retaining(&mut owner.children, &mut command)?; drop(inherited); drop(service_child_gate);
        let original_service = owner.children[service_index].original()?;
        // Bind the ACTUAL proc-exe file to this still-live original service
        // pidfd before handing either witness to the UID1500 engine.
        original_service.live()?; image.matches_process(original_service)?;
        let service_exe: OwnedFd = File::open(format!("/proc/{}/exe", original_service.pid))?.into();
        fd_cloexec(&service_exe)?; image.matches_fd(&service_exe)?; image.matches_process(original_service)?; original_service.live()?;
        owner.service_gate = Some(Gate::capture(service_parent_gate, original_service.fd().try_clone()?, 0, &nonce, &original)?);
        let gate = owner.service_gate.as_mut().unwrap();
        gate.send("service_endpoint", serde_json::json!({}), Some(&service_endpoint))?;
        gate.send("engine_pidfd", serde_json::json!({}), Some(owner.children[engine_index].original()?.fd()))?;
        let (_, fd) = gate.expect("service_ready")?; no_fd(fd)?; drop(service_endpoint);
        let gate = owner.engine_gate.as_mut().unwrap();
        gate.send("service_pidfd", serde_json::json!({}), Some(owner.children[service_index].original()?.fd()))?;
        gate.send("service_exe", serde_json::json!({}), Some(&service_exe))?; drop(service_exe);
        let (enrolled, fd) = gate.expect("helpers_enrolled")?; no_fd(fd)?;
        let before = snapshot(owner.service_gate.as_mut().unwrap())?; let helper_pids = check_helpers(&before,0)?;
        ensure!(enrolled["helpers"].as_array().context("fixture engine helper list absent")?.iter().map(|p|p.as_u64().unwrap_or(0) as u32).collect::<BTreeSet<_>>()
            == helper_pids.iter().copied().collect::<BTreeSet<_>>(), "fixture enrolled helper identities differ");
        owner.engine_gate.as_mut().unwrap().send("release_views", serde_json::json!({}), None)?;
        let (_, fd) = owner.engine_gate.as_mut().unwrap().expect("one_helper_exited")?; no_fd(fd)?;
        let one = snapshot(owner.service_gate.as_mut().unwrap())?; check_helpers(&one,1)?;
        owner.engine_gate.as_mut().unwrap().send("finish_second", serde_json::json!({}), None)?;
        let (_, cancel_fd) = owner.engine_gate.as_mut().unwrap().expect("cancel_ready")?;
        let cancel_fd = cancel_fd.context("fixture original cancel event not transferred")?;
        let (_, fd) = owner.service_gate.as_mut().unwrap().expect("second_begin_paused")?; no_fd(fd)?;
        cancel_event(&cancel_fd);
        owner.service_gate.as_mut().unwrap().send("resume_cancelled", serde_json::json!({}), None)?;
        let (_, fd) = owner.service_gate.as_mut().unwrap().expect("cancel_observed")?; no_fd(fd)?;
        let (engine_proof, fd) = owner.engine_gate.as_mut().unwrap().expect("engine_complete")?; no_fd(fd)?;
        let final_snapshot = snapshot(owner.service_gate.as_mut().unwrap())?; check_helpers(&final_snapshot,2)?;
        ensure!(published_slot(&final_snapshot)?["seed"] == engine_proof["seed"]
            && published_slot(&final_snapshot)?["baseline"] == engine_proof["baseline"]
            && published_slot(&final_snapshot)?["operation"] == engine_proof["operation"], "fixture transferred exact seed/history identity differs");
        let slots = final_snapshot["slots"].as_array().context("fixture retained slot set absent")?;
        ensure!(slots.len() == 2, "fixture cancellation original slot not retained");
        let failed = slots.iter().find(|s|s["published"] == false).context("fixture failed original slot absent")?;
        ensure!(failed["fenced"] == true && failed["stage_owner_retained"] == true && failed["cancel_event_retained"] == true
            && failed["cancel_flag"] == true && failed["original_budget_cancelled"] == true && failed["cancel_observer_handle_retained"] == true
            && failed["mount_owner_retained"] == false && failed["root_cas_uid"] == 0, "fixture cancelled/error owners discarded or mounted");
        ensure!(engine_proof["helpers_successfully_reaped"] == 2 && engine_proof["helper_pipe_eof"] == true
            && engine_proof["source_cas_uid"] == 1500 && engine_proof["client_source_pins_retained"] == 2
            && engine_proof["production_template"] == true && engine_proof["private_forks"] == 2
            && engine_proof["real_git_history"] == true && engine_proof["second_fork_unchanged"] == true
            && engine_proof["normal_retirement_blocked"] == true && engine_proof["cancelled_transfer_failed"] == true,
            "fixture engine component checks differ");
        ensure!(published_slot(&final_snapshot)?["root_cas_identity"] != engine_proof["source_cas_identity"], "fixture Root/UID1500 CAS identity is not independent");
        owner.engine_gate.as_mut().unwrap().send("confirm_retained", serde_json::json!({}), None)?;
        let (retained, fd) = owner.engine_gate.as_mut().unwrap().expect("retained")?; no_fd(fd)?;
        ensure!(retained["client_and_source_owners_retained"] == true && retained["dumpable"] == 0, "fixture original engine owners no longer retained");
        owner.children[service_index].original()?.live()?; owner.children[engine_index].original()?.live()?;
        image.matches_process(owner.children[service_index].original()?)?; image.matches_process(owner.children[engine_index].original()?)?;
        remaining(&original)?;
        Ok(serde_json::json!({"format_version":1,"scope":SCOPE,"nonce":nonce,"pinned_elf_sha256":image.sha256,
            "root_service_pid":owner.children[service_index].original()?.pid,"engine_pid":owner.children[engine_index].original()?.pid,
            "service_and_engine_actual_pidfds_live":true,"service_and_engine_owners_retained_until_vm_teardown":true,
            "actual_helper_pids":helper_pids,"helpers_successfully_reaped":2,"helper_pipe_eof":true,
            "one_helper_exit_kept_sibling_custody":true,"actual_scm_credentials":true,"actual_proc_exe_bootstrap_binding":true,
            "root_sys_ptrace_and_host_namespace_checks":true,"root_and_uid1500_erofs":true,
            "independent_root_cas":true,"cancelled_error_owners_retained":true,"root_snapshot":final_snapshot,"engine":engine_proof,
            "normal_retirement_blocked":true,"normal_cleanup_performed":false,"production_launcher":false,"spec3":false,"backend":false,"models":false}))
    })).unwrap_or_else(|_|Err(anyhow::anyhow!("fixture root parent panicked; actual child carriers retained")));
    // The actual background role owners outlive this foreground command and
    // remain on the exact owned VM. This is deliberately NOT normal cleanup.
    let _retained_parent = Box::leak(owner);
    match result { Ok(proof) => { println!("{}", proof); Ok(()) }, Err(_) => anyhow::bail!("root IPC component qualification failed; retained roles require owned VM teardown") }
}

fn helper_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut raw = [-1; 2];
    ensure!(unsafe { libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,raw.as_mut_ptr()) } == 0,
        "fixture helper gate pair unavailable");
    let first = unsafe { OwnedFd::from_raw_fd(raw[0]) }; let second = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    for fd in [&first,&second] { let yes = 1i32; ensure!(unsafe { libc::setsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,
        (&yes as *const i32).cast(),std::mem::size_of::<i32>() as _) } == 0, "fixture helper gate credentials unavailable"); }
    Ok((first,second))
}
struct EngineRoleOwner {
    gate: Gate, client: Option<RootStorageClient>, source: Option<SourceOwner>, prepared: Option<PreparedInitialSource>,
    store: Option<LocalStore>, first: Option<ClientSeedOwner>, failed: Option<ClientSeedOwner>,
    template: Option<VerifiedMetadataTemplate>, children: Vec<ChildOwner>, helper_gates: Vec<Gate>,
    helper_tokens: Vec<crate::root_storage_client::EnrolledSeedHelper>,
}
fn client_seed_pins(store: &LocalStore, seed: &ObjectRef) -> Result<usize> {
    Ok(store.retention()?.pins.iter().filter(|(name,r)| name.starts_with("root-storage-client:") && *r == seed).count())
}
fn engine_owners_retained(owner: &EngineRoleOwner) -> Result<serde_json::Value> {
    owner.source.as_ref().context("fixture source owner absent")?.check()?;
    let prepared = owner.prepared.as_ref().context("fixture prepared owner absent")?;
    let store = owner.store.as_ref().context("fixture user CAS owner absent")?;
    ensure!(owner.client.is_some() && owner.first.is_some() && owner.failed.is_some() && owner.template.is_some()
        && owner.helper_tokens.len() == 2 && client_seed_pins(store,&prepared.seed_root)? == 2,
        "fixture original client/source owners discarded");
    ensure!(prepared.repository().op_id().hex() == prepared.operation.0, "fixture original prepared operation changed");
    Ok(serde_json::json!({"client_and_source_owners_retained":true,"dumpable":unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)}}))
}

#[test]
#[ignore = "Only paired root IPC parent may invoke the retained UID1500 engine role"]
fn root_storage_ipc_engine_role() -> Result<()> {
    let (nonce, original) = role_context(1500)?;
    let storage_endpoint = take_inherited(3)?;
    let gate = Gate::capture(take_inherited(4)?,take_inherited(5)?,0,&nonce,&original)?;
    ensure!(unsafe {libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)} == 0
        && unsafe {libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)} == 0, "fixture early engine nondumpable protection unavailable");
    let image = Image::capture(Path::new(ENGINE_IMAGE))?; let own = Process::current()?; image.matches_process(&own)?;
    let mut owner = EngineRoleOwner { gate,client:None,source:None,prepared:None,store:None,first:None,failed:None,
        template:None,children:Vec::new(),helper_gates:Vec::new(),helper_tokens:Vec::new() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<serde_json::Value> {
        owner.gate.send("engine_waiting",serde_json::json!({"pid":own.pid,"dumpable":0}),None)?;
        let (_,service_pidfd) = owner.gate.expect("service_pidfd")?;
        let (_,service_image) = owner.gate.expect("service_exe")?;
        owner.client = Some(RootStorageClient::from_bootstrap(storage_endpoint,
            service_pidfd.context("fixture original service pidfd absent")?,service_image.context("fixture actual service exe witness absent")?)?);
        owner.client.as_mut().unwrap().authenticate_original_service(&original)?;
        let scratch = std::env::current_dir()?.canonicalize()?; let scratch_owner = HeldDir::user(&scratch,0o700)?;
        let settings = fixture_settings()?;
        owner.source = Some(pollster::block_on(make_source(&scratch,&settings,&original))?);
        owner.store = Some(LocalStore::open(scratch.join("uid-cas"))?);
        let store = owner.store.as_ref().unwrap().clone();
        let empty = store.put_directory(DirectoryManifest {format_version:1,mode:0o755,entries:vec![]})?;
        let environment = store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2 {format_version:2,directory:empty,
            platform:BTreeMap::from([("fixture_abi".into(),"linux-root-ipc".into())]),installer_inputs:vec![],provenance:BTreeMap::new()})?;
        store.pin("root-ipc-environment",&environment)?;
        let integration = scratch.join("private-integration"); fs::DirBuilder::new().mode(0o700).create(&integration)?;
        let export = scratch.join("seed-export"); let mut owner_checks = 0usize;
        let source = owner.source.as_ref().unwrap();
        owner.prepared = Some(pollster::block_on(prepare_initial_source(store.clone(),InitialSourceRequest {
            source_git:&source.git.path,private_repository:&integration,seed_destination:&export,
            expected_head:&source.head.commit,environment:&environment,settings:&settings,
        },&original,|| {owner_checks+=1;source.check()}))?);
        let prepared = owner.prepared.as_ref().unwrap();
        ensure!(owner_checks>0 && prepared.baseline.commit==source.head.commit && prepared.baseline.parents==source.head.parents
            && prepared.baseline.tree==source.head.tree && prepared.baseline.parents.len()==1, "fixture real source history changed");
        let runnable = decode_runnable(&store.read_object(&prepared.runnable_root)?)?;
        let source_root = decode_source(&store.read_object(&prepared.source_root)?)?;
        ensure!(runnable.source==prepared.source_root && runnable.view_root==source_root.directory, "fixture real source/runnable mapping differs");
        let seed = prepared.seed_root.clone(); let baseline = prepared.baseline.clone(); let operation = prepared.operation.clone();
        let profile = runnable.profile; let policy = source_root.projection_policy;
        owner.first = Some(ClientSeedOwner::capture(store.clone(),seed.clone())?);
        owner.client.as_mut().unwrap().prepare_seed_retaining(owner.first.as_mut().unwrap(),&original)?;
        owner.template = Some(owner.client.as_mut().unwrap().open_retained_template(owner.first.as_ref().unwrap(),&original)?);
        ensure!(owner.template.as_ref().unwrap().manifest().baseline==baseline
            && owner.template.as_ref().unwrap().manifest().operation==operation, "fixture production client template differs");
        let (root,objects) = owner.first.as_ref().unwrap().paths().context("fixture published root paths absent")?;
        let root = root.to_path_buf(); let objects = objects.to_path_buf(); erofs_head(&root)?;
        for id in ["a","b"] {
            let (parent_gate,child_gate) = helper_pair()?;
            let (mut command,inherited) = role_command("root_storage_ipc_helper_role",false,true,&scratch,&nonce,&original,
                &[(&child_gate,3),(own.fd(),4)])?;
            let index = start_retaining(&mut owner.children,&mut command)?; drop(inherited); drop(child_gate);
            let helper = owner.children[index].original()?;
            let mut gate = Gate::capture(parent_gate,helper.fd().try_clone()?,1500,&nonce,&original)?;
            let (_,fd) = gate.expect("helper_waiting")?; no_fd(fd)?;
            // This fixture owns the actual Child + original first-open pidfd.
            // Production uses the separately tested HelperProcessWitness export.
            let token = owner.client.as_mut().unwrap().enroll_original_helper_retaining(owner.first.as_mut().unwrap(),helper.fd().try_clone()?,&original)?;
            owner.helper_tokens.push(token); owner.helper_gates.push(gate);
            ensure!(id=="a" || id=="b", "fixture helper id invalid");
        }
        let pids = owner.children.iter().map(|h|Ok(h.original()?.pid)).collect::<Result<Vec<_>>>()?;
        ensure!(pids.len()==2 && pids[0]!=pids[1], "fixture original helper children not distinct");
        owner.gate.send("helpers_enrolled",serde_json::json!({"helpers":pids}),None)?;
        let (_,fd) = owner.gate.expect("release_views")?; no_fd(fd)?;
        for (index,id) in ["a","b"].into_iter().enumerate() {
            owner.helper_gates[index].send("release_view",serde_json::json!({"id":id,"root":root,"objects":objects,
                "store":store.root(),"seed":seed,"baseline":baseline,"operation":operation,"profile":profile,"policy":policy}),None)?;
        }
        let (_,fd) = owner.helper_gates[1].expect("view_ready")?; no_fd(fd)?;
        let (first_proof,fd) = owner.helper_gates[0].expect("helper_complete")?; no_fd(fd)?;
        owner.helper_gates[0].send("reap_now",serde_json::json!({}),None)?;
        owner.children[0].await_exit_and_pipes(&original)?;
        ensure!(!owner.client.as_ref().unwrap().all_enrolled_helpers_exited(owner.first.as_ref().unwrap())?,
            "fixture one original helper exit replaced live sibling custody");
        ensure!(owner.client.as_ref().unwrap().enrolled_helper_exited(owner.first.as_ref().unwrap(),&owner.helper_tokens[0])?
            && !owner.client.as_ref().unwrap().enrolled_helper_exited(owner.first.as_ref().unwrap(),&owner.helper_tokens[1])?,
            "fixture original per-helper exit token differs");
        let new_baseline: CommitIdentity = serde_json::from_value(first_proof["new_baseline"].clone())?;
        let new_operation: JjOperationId = serde_json::from_value(first_proof["new_operation"].clone())?;
        new_baseline.validate()?;new_operation.validate()?;
        ensure!(new_baseline.commit!=baseline.commit && new_baseline.change!=baseline.change && new_baseline.tree!=baseline.tree
            && new_baseline.parents==vec![baseline.commit.clone()] && new_operation!=operation,"fixture first private fork did not change");
        owner.gate.send("one_helper_exited",serde_json::json!({}),None)?;
        let (_,fd) = owner.gate.expect("finish_second")?; no_fd(fd)?;
        owner.helper_gates[1].send("finish_view",serde_json::json!({"new_commit":new_baseline.commit}),None)?;
        let (second_proof,fd) = owner.helper_gates[1].expect("helper_complete")?; no_fd(fd)?;
        owner.helper_gates[1].send("reap_now",serde_json::json!({}),None)?;
        owner.children[1].await_exit_and_pipes(&original)?;
        ensure!(second_proof["second_fork_unchanged"]==true
            && owner.client.as_ref().unwrap().all_enrolled_helpers_exited(owner.first.as_ref().unwrap())?,"fixture all original helpers not physically exited");
        pollster::block_on(async {
            let _first = reopen(&scratch.join("fork-a"),&new_baseline,&new_operation,MUTATED_BYTES,ACCEPTED_BYTES,&original).await?;
            let second = reopen(&scratch.join("fork-b"),&baseline,&operation,ACCEPTED_BYTES,PARENT_BYTES,&original).await?;
            let new_id = CommitId::try_from_hex(&new_baseline.commit.0).context("fixture new commit ID invalid")?;
            ensure!(second.store().get_commit(&new_id).is_err(),"fixture second fork sees first fork's private object");
            Ok::<_,anyhow::Error>(())
        })?;
        ensure!(owner.client.as_mut().unwrap().request_retirement_retaining(owner.first.as_mut().unwrap(),&original).is_err(),
            "fixture invented a normal actor-drain retirement proof");
        owner.failed = Some(ClientSeedOwner::capture(store.clone(),seed.clone())?);
        let cancel = owner.failed.as_ref().unwrap().qualification_cancel_fd()?;
        owner.gate.send("cancel_ready",serde_json::json!({}),Some(&cancel))?;
        let failed = owner.client.as_mut().unwrap().prepare_seed_retaining(owner.failed.as_mut().unwrap(),&original);
        ensure!(failed.is_err() && owner.failed.as_ref().unwrap().paths().is_none(),"fixture actual cancelled transfer returned success");
        ensure!(owner.client.as_ref().unwrap().all_enrolled_helpers_exited(owner.first.as_ref().unwrap()).is_err(),
            "fixture failed exchange did not fence original client");
        source.check()?; scratch_owner.check()?;
        let m = fs::symlink_metadata(store.root())?;
        ensure!(m.uid()==1500 && m.gid()==1500 && m.mode() & 0o7777==0o700 && client_seed_pins(&store,&seed)?==2,
            "fixture failed transfer changed/discarded UID1500 source retention");
        Ok(serde_json::json!({"seed":seed,"baseline":baseline,"operation":operation,"new_baseline":new_baseline,"new_operation":new_operation,
            "source_owner_checks":owner_checks,"source_cas_uid":m.uid(),"source_cas_identity":[m.dev(),m.ino()],
            "client_source_pins_retained":2,"production_template":true,"private_forks":2,"real_git_history":true,
            "second_fork_unchanged":true,"helpers_successfully_reaped":2,"helper_pipe_eof":true,
            "normal_retirement_blocked":true,"cancelled_transfer_failed":true,"client_fenced_after_error":true,"uid1500_erofs":true}))
    })).unwrap_or_else(|_|Err(anyhow::anyhow!("fixture engine role panicked; actual owners retained")));
    match result {
        Ok(proof) => {
            if owner.gate.send("engine_complete",proof,None).is_err() { retained_forever(&owner); }
            loop {
                match owner.gate.receive() {
                    Ok(Some((label,_,fd))) if label=="confirm_retained" && fd.is_none() => {
                        match engine_owners_retained(&owner) {
                            Ok(proof) => if owner.gate.send("retained",proof,None).is_err() { retained_forever(&owner); },
                            Err(_) => retained_forever(&owner),
                        }
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                    _ => retained_forever(&owner),
                }
            }
        }
        Err(_) => { let _=owner.gate.send("role_failed",serde_json::json!({"original_owners_retained":true}),None); retained_forever(&owner) }
    }
}

#[test]
#[ignore = "Only enrolled UID1500 engine may release this actual helper's private test use barrier"]
fn root_storage_ipc_helper_role() -> Result<()> {
    let (nonce,original)=role_context(1500)?;
    let mut gate=Gate::capture(take_inherited(3)?,take_inherited(4)?,1500,&nonce,&original)?;
    let own=Process::current()?;Image::capture(Path::new(ENGINE_IMAGE))?.matches_process(&own)?;
    gate.send("helper_waiting",serde_json::json!({"pid":own.pid}),None)?;
    let (body,fd)=gate.expect("release_view")?;no_fd(fd)?;
    let id=body["id"].as_str().context("fixture helper view id absent")?;ensure!(id=="a"||id=="b","fixture helper view id invalid");
    let root=PathBuf::from(body["root"].as_str().context("fixture sealed root absent")?);
    let objects=PathBuf::from(body["objects"].as_str().context("fixture sealed objects absent")?);
    ensure!(objects==root.join("objects"),"fixture helper objects differ");
    let store=LocalStore::open(Path::new(body["store"].as_str().context("fixture user CAS absent")?))?;
    ensure!(fs::symlink_metadata(store.root())?.uid()==1500,"fixture helper opened wrong CAS owner");
    let seed=value_ref(&body,"seed")?;let baseline:CommitIdentity=serde_json::from_value(body["baseline"].clone())?;
    let operation:JjOperationId=serde_json::from_value(body["operation"].clone())?;
    let profile=value_ref(&body,"profile")?;let policy=value_ref(&body,"policy")?;
    let profile_value=WorkspaceProfile::source_only();
    ensure!(profile==ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&profile_value)?)
        && policy==SourceProjectionPolicy::from_import_policy(&profile_value.source_exclusions())?.reference()?,"fixture helper profile/policy differs");
    let template=VerifiedMetadataTemplate::open_retained(&store,&seed,&root,&objects)?;
    ensure!(template.manifest().baseline==baseline && template.manifest().operation==operation,"fixture helper retained template differs");erofs_head(&root)?;
    let scratch=std::env::current_dir()?.canonicalize()?;let path=scratch.join(format!("fork-{id}"));
    let binding=AttemptBinding {format_version:1,project_id:"root-ipc-physical-fixture".into(),owner:OwnerEpoch {owner:"root-ipc".into(),epoch:1},
        run_id:id.into(),run_epoch:1,attempt_id:id.into(),view_id:id.into(),seed:seed.clone(),baseline:baseline.clone(),baseline_operation:operation.clone(),
        projection_policy:policy,profile};
    let fork=fork_private_metadata(&template,&path,binding,None)?;
    ensure!(fork.kernel_readonly_objects && fs::read_to_string(path.join(".git/objects/info/alternates"))?==format!("{}\n",objects.display()),
        "fixture helper private fork lost shared RO objects");
    let original_repo=pollster::block_on(reopen(&path,&baseline,&operation,ACCEPTED_BYTES,PARENT_BYTES,&original))?;
    if id=="a" {
        let (new_baseline,new_operation)=pollster::block_on(async {
            let accepted=original_repo.store().get_commit(&CommitId::try_from_hex(&baseline.commit.0).context("fixture accepted ID invalid")?)?;
            let tree=changed_tree(&original_repo,accepted.tree(),MUTATED_BYTES,&original).await?;
            let mut tx=original_repo.start_transaction();
            let commit=tx.repo_mut().new_commit(vec![accepted.id().clone()],tree).set_description("only first Root IPC private view").clear_sign_key().write().await?;
            tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone())?;
            tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(commit.id().clone()));
            let changed=tx.commit("first Root IPC private operation").await?;
            let baseline=CommitIdentity::from_jj(&commit);let operation=JjOperationId(changed.op_id().hex());
            write_head(&path.join(".git/HEAD"),&baseline.commit)?;
            let _reopened=reopen(&path,&baseline,&operation,MUTATED_BYTES,ACCEPTED_BYTES,&original).await?;
            Ok::<_,anyhow::Error>((baseline,operation))
        })?;
        gate.send("helper_complete",serde_json::json!({"new_baseline":new_baseline,"new_operation":new_operation}),None)?;
    } else {
        gate.send("view_ready",serde_json::json!({}),None)?;
        let (finish,fd)=gate.expect("finish_view")?;no_fd(fd)?;
        let new_commit:GitCommitId=serde_json::from_value(finish["new_commit"].clone())?;new_commit.validate()?;
        let reopened=pollster::block_on(reopen(&path,&baseline,&operation,ACCEPTED_BYTES,PARENT_BYTES,&original))?;
        ensure!(reopened.store().get_commit(&CommitId::try_from_hex(&new_commit.0).context("fixture first private commit invalid")?).is_err(),
            "fixture second original helper can see first private object");
        gate.send("helper_complete",serde_json::json!({"second_fork_unchanged":true}),None)?;
    }
    ensure!(fs::read(root.join("metadata/.git/HEAD"))?==format!("{}\n",baseline.commit.0).as_bytes(),"fixture private helper changed sealed seed");
    let (_,fd)=gate.expect("reap_now")?;no_fd(fd)?;
    remaining(&original)?;Ok(())
}
