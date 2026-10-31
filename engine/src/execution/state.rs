//! Local receipt authority, independent of the project journal and CAS pins.
use super::*;
use anyhow::{ensure, Context};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub(crate) struct State {
    pub directory: PathBuf,
    pub record: ExecutionRecord,
    pub is_new: bool,
    manager_root: PathBuf,
    store_root: PathBuf,
    workspace_attempted: bool,
    lock_identity: LockIdentity,
    _lock: File,
}
/// Both variants keep the actual lock. Historical data never invokes a new
/// service/workspace factory or restores the old process/session authority.
pub(crate) enum Preflight {
    Recovered(State),
    New {
        state: State,
        services: Option<HelperServices>,
    },
}
pub(crate) fn preflight(
    spec: &ExecutionSpec,
    factory: Option<HelperServicesFactory>,
) -> Result<Preflight> {
    let mut state = State::open(spec)?;
    if !state.is_new {
        ensure!(
            state.record.phase == ExecutionPhase::Checkpointed
                && state.record.checkpoint.is_some()
                && state.record.agent_exit.is_some(),
            "execution has uncertain prior effects; reconcile its orphan before retry"
        );
        let checkpoint = state.record.checkpoint.as_ref().unwrap();
        checkpoint.directory.validate()?;
        checkpoint.hardlinks.validate()?;
        ensure!(
            checkpoint.format_version == 1
                && checkpoint.directory.kind == crate::protocol::ObjectKind::Directory
                && checkpoint.hardlinks.kind == crate::protocol::ObjectKind::Blob,
            "recovered execution has invalid complete checkpoint metadata"
        );
        state.check_lock()?;
        return Ok(Preflight::Recovered(state));
    }
    let services = (|| -> Result<Option<HelperServices>> {
        if spec.workspace_observation.is_some() {
            Ok(Some(factory.context(
                "workspace helper lacks a qualified authority factory",
            )?(spec)?))
        } else {
            Ok(None)
        }
    })();
    match services {
        Ok(services) => {
            state.check_lock()?;
            Ok(Preflight::New { state, services })
        }
        Err(error) => {
            state.record.phase = ExecutionPhase::Orphaned;
            let _ = state.save();
            Err(error)
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    record: ExecutionRecord,
    checksum: String,
}
/// Private new-reservation evidence. It never establishes liveness or an
/// attempt grant; admission also requires the actual held lock and authority.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockIdentity {
    format_version: u32,
    execution_id: String,
    operation_id: String,
    view_id: String,
    owner: OwnerEpoch,
    request_digest: String,
    directory_device: u64,
    directory_inode: u64,
    device: u64,
    inode: u64,
    nonce: String,
}
impl State {
    pub fn open(spec: &ExecutionSpec) -> Result<Self> {
        spec.validate()?;
        // These are path identities only. No VFS manager, LocalStore, remote
        // process, view or authority factory is opened during preflight.
        let manager_root = canonical_directory_or_future(&spec.manager_root)?;
        let store_root = canonical_directory_or_future(&spec.store_root)?;
        let root = &spec.execution_root;
        ensure!(
            root.is_absolute() && root != Path::new("/"),
            "invalid private execution root"
        );
        let marker = root.join("execution-manager.json");
        if !root.try_exists()? {
            ensure!(
                root.parent().context("execution root parent")?.is_dir(),
                "execution root parent must already exist"
            );
            fs::create_dir(root)?;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            File::open(root.parent().unwrap())?.sync_all()?;
        }
        let m = fs::symlink_metadata(root)?;
        ensure!(
            m.is_dir()
                && !m.file_type().is_symlink()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0,
            "execution root is not a private owned real directory"
        );
        let expected = crate::protocol::canonical_json(
            &serde_json::json!({"format_version":1,"manager_root":manager_root,"store_root":store_root}),
        )?;
        if let Some(bytes) = read_private(&marker, 64 * 1024)? {
            ensure!(bytes == expected, "execution manager binding differs");
        } else {
            ensure!(
                fs::read_dir(root)?.next().is_none(),
                "refusing nonempty non-execution directory"
            );
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&marker)?;
            f.write_all(&expected)?;
            f.sync_all()?;
            File::open(root)?.sync_all()?;
        }
        for name in ["runs"] {
            private_state_directory(&root.join(name))?;
        }
        let directory = root.join("runs").join(&spec.execution_id);
        private_state_directory(&directory)?;
        let digest = hex::encode(Sha256::digest(crate::protocol::canonical_json(spec)?));
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let lock_path = directory.join("owner.lock");
        let (mut lock, new_lock) = match fs::symlink_metadata(&lock_path) {
            Ok(_) => (lock_options.open(&lock_path)?, false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    fs::read_dir(&directory)?.next().is_none(),
                    "execution lock is missing beside retained evidence; explicit recovery required"
                );
                (lock_options.create_new(true).open(&lock_path)?, true)
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = lock.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "unsafe execution owner lock"
        );
        lock.try_lock_exclusive()
            .context("execution already has a live supervisor")?;
        let directory_metadata = fs::symlink_metadata(&directory)?;
        let identity_path = directory.join("lock-identity.json");
        let lock_identity = if new_lock {
            let nonce = Uuid::new_v4().to_string();
            lock.write_all(nonce.as_bytes())?;
            lock.sync_all()?;
            let identity = LockIdentity {
                format_version: 1,
                execution_id: spec.execution_id.clone(),
                operation_id: spec.operation_id.clone(),
                view_id: spec.view_id.clone(),
                owner: spec.owner.clone(),
                request_digest: digest.clone(),
                directory_device: directory_metadata.dev(),
                directory_inode: directory_metadata.ino(),
                device: metadata.dev(),
                inode: metadata.ino(),
                nonce,
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&identity_path)?;
            file.write_all(&crate::protocol::canonical_json(&identity)?)?;
            file.sync_all()?;
            File::open(&directory)?.sync_all()?;
            identity
        } else {
            let bytes = read_private(&identity_path, 16 * 1024)?.context(
                "execution lacks lock-identity evidence; original data is historical recovery-only",
            )?;
            let identity: LockIdentity = serde_json::from_slice(&bytes)?;
            ensure!(
                identity.format_version == 1
                    && identity.execution_id == spec.execution_id
                    && identity.operation_id == spec.operation_id
                    && identity.view_id == spec.view_id
                    && identity.owner == spec.owner
                    && identity.request_digest == digest,
                "execution lock identity differs from exact request; explicit recovery required"
            );
            identity
        };
        check_lock_identity(&directory, &lock, &lock_identity)?;
        let path = directory.join("record.json");
        let persisted = read_private(&path, 1024 * 1024)?;
        ensure!(
            persisted.is_some() || new_lock,
            "execution record is missing after a prior reservation; explicit reconciliation required"
        );
        let is_new = persisted.is_none();
        let record = if let Some(bytes) = persisted {
            let envelope: Envelope = serde_json::from_slice(&bytes)?;
            ensure!(
                hex::encode(Sha256::digest(crate::protocol::canonical_json(
                    &envelope.record
                )?)) == envelope.checksum,
                "execution record checksum differs"
            );
            let r = envelope.record;
            ensure!(
                r.format_version == 1
                    && r.request_digest == digest
                    && r.owner == spec.owner
                    && r.view_id == spec.view_id
                    && r.operation_id == spec.operation_id
                    && r.execution_id == spec.execution_id,
                "execution id is bound to different inputs/owner"
            );
            r
        } else {
            ExecutionRecord {
                format_version: 1,
                execution_id: spec.execution_id.clone(),
                operation_id: spec.operation_id.clone(),
                view_id: spec.view_id.clone(),
                owner: spec.owner.clone(),
                request_digest: digest,
                phase: ExecutionPhase::Preparing,
                supervisor_pid: std::process::id(),
                agent_exit: None,
                checkpoint: None,
            }
        };
        let state = Self {
            directory,
            record,
            is_new,
            manager_root,
            store_root,
            workspace_attempted: false,
            lock_identity,
            _lock: lock,
        };
        if state.is_new {
            state.save()?;
        }
        Ok(state)
    }
    fn new_execution<'a>(&'a self, spec: &'a ExecutionSpec) -> Result<NewExecution<'a>> {
        self.check_lock()?;
        ensure!(
            self.is_new
                && self.record.phase == ExecutionPhase::Preparing
                && self.record.request_digest
                    == hex::encode(Sha256::digest(crate::protocol::canonical_json(spec)?)),
            "only the exact newly locked execution may construct a workspace"
        );
        Ok(NewExecution::locked(
            spec,
            &self.directory,
            &self.manager_root,
            &self.store_root,
            &self.record.request_digest,
            &self._lock,
        ))
    }
    pub(crate) fn open_workspace(
        &mut self,
        spec: &ExecutionSpec,
        factory: Option<HelperWorkspaceFactory>,
    ) -> Result<OwnedHelperWorkspace> {
        ensure!(
            !self.workspace_attempted,
            "workspace factory already attempted for this execution"
        );
        self.new_execution(spec)?;
        self.workspace_attempted = true;
        let request = self.new_execution(spec)?;
        let workspace = match factory {
            Some(factory) => factory(&request),
            None => OwnedHelperWorkspace::local(&request),
        }?;
        self.check_lock()?;
        Ok(workspace)
    }
    pub(crate) fn checkpoint_workspace(
        &self,
        workspace: &OwnedHelperWorkspace,
    ) -> Result<VfsCheckpoint> {
        // A named-lock/directory/sidecar change retires this execution even
        // after the workspace opened. Check before CAS/staging/retention effects;
        // the post-check withholds publication if custody changes mid-capture.
        self.check_lock()?;
        workspace.require_supported_execution()?;
        let checkpoint = workspace.view().checkpoint_snapshot_unmounted()?;
        self.check_lock()?;
        Ok(checkpoint)
    }
    pub fn save(&self) -> Result<()> {
        self.check_lock()?;
        let record = self.record.clone();
        let envelope = Envelope {
            checksum: hex::encode(Sha256::digest(crate::protocol::canonical_json(&record)?)),
            record,
        };
        let bytes = crate::protocol::canonical_json(&envelope)?;
        let temp = self.directory.join(format!(".record-{}", Uuid::new_v4()));
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(temp, self.directory.join("record.json"))?;
        File::open(&self.directory)?.sync_all()?;
        self.check_lock()?;
        Ok(())
    }
    fn check_lock(&self) -> Result<()> {
        check_lock_identity(&self.directory, &self._lock, &self.lock_identity)
    }
}

fn private_state_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "unsafe private execution state directory; explicit recovery required"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            File::open(path)?.sync_all()?;
            File::open(path.parent().context("execution state directory parent")?)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn check_lock_identity(directory: &Path, lock: &File, identity: &LockIdentity) -> Result<()> {
    let held = lock.metadata()?;
    let named = fs::symlink_metadata(directory.join("owner.lock"))?;
    let directory_metadata = fs::symlink_metadata(directory)?;
    ensure!(
        directory_metadata.is_dir()
            && !directory_metadata.file_type().is_symlink()
            && directory_metadata.uid() == unsafe { libc::geteuid() }
            && directory_metadata.mode() & 0o077 == 0
            && directory_metadata.dev() == identity.directory_device
            && directory_metadata.ino() == identity.directory_inode,
        "execution directory identity changed; explicit recovery required"
    );
    for metadata in [&held, &named] {
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1
                && metadata.dev() == identity.device
                && metadata.ino() == identity.inode
                && metadata.len() == 36,
            "execution lock identity changed; explicit recovery required"
        );
    }
    ensure!(
        Uuid::parse_str(&identity.nonce)?.to_string() == identity.nonce,
        "invalid execution lock nonce"
    );
    let mut nonce = [0u8; 36];
    std::os::unix::fs::FileExt::read_exact_at(lock, &mut nonce, 0)?;
    ensure!(
        nonce == identity.nonce.as_bytes(),
        "execution lock nonce differs"
    );
    let expected = crate::protocol::canonical_json(identity)?;
    ensure!(
        read_private(&directory.join("lock-identity.json"), 16 * 1024)?
            .is_some_and(|bytes| bytes == expected),
        "execution lock sidecar changed; explicit recovery required"
    );
    Ok(())
}

pub(super) fn read_recovery_record(
    root: &Path,
    execution_id: &str,
) -> Result<Option<ExecutionRecord>> {
    ensure!(
        root.is_absolute()
            && root != Path::new("/")
            && root.components().all(|part| matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "historical execution root requires a normalized absolute path"
    );
    ensure!(
        Uuid::parse_str(execution_id)?.to_string() == execution_id,
        "historical execution id must be a canonical UUID"
    );
    let runs = root.join("runs");
    let directory = runs.join(execution_id);
    for path in [root, runs.as_path(), directory.as_path()] {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "historical execution directory has unsafe type/owner/mode"
        );
    }
    let Some(bytes) = read_private(&directory.join("record.json"), 1024 * 1024)? else {
        return Ok(None);
    };
    let envelope: Envelope = serde_json::from_slice(&bytes)?;
    ensure!(
        hex::encode(Sha256::digest(crate::protocol::canonical_json(
            &envelope.record
        )?)) == envelope.checksum,
        "historical execution checksum differs"
    );
    ensure!(
        envelope.record.format_version == 1 && envelope.record.execution_id == execution_id,
        "historical execution record identity differs"
    );
    envelope.record.owner.validate()?;
    for id in [&envelope.record.operation_id, &envelope.record.view_id] {
        ensure!(
            Uuid::parse_str(id)?.to_string() == *id,
            "invalid historical execution identity"
        );
    }
    if let Some(checkpoint) = &envelope.record.checkpoint {
        checkpoint.directory.validate()?;
        checkpoint.hardlinks.validate()?;
        ensure!(
            checkpoint.format_version == 1
                && checkpoint.directory.kind == crate::protocol::ObjectKind::Directory
                && checkpoint.hardlinks.kind == crate::protocol::ObjectKind::Blob,
            "historical checkpoint metadata differs"
        );
    }
    Ok(Some(envelope.record))
}

fn read_private(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= limit,
        "unsafe or oversized private execution record"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "private execution record exceeds bound"
    );
    Ok(Some(bytes))
}
fn canonical_directory_or_future(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute()
            && path.components().all(|part| matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "execution workspace requires normalized absolute paths"
    );
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    missing.len() < 128,
                    "execution workspace path exceeds bounded depth"
                );
                missing.push(
                    existing
                        .file_name()
                        .context("workspace path has no existing ancestor")?
                        .to_os_string(),
                );
                ensure!(existing.pop(), "workspace path has no existing ancestor");
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut canonical = existing.canonicalize()?;
    ensure!(canonical.is_dir(), "workspace ancestor is not a directory");
    for part in missing.into_iter().rev() {
        canonical.push(part);
    }
    Ok(canonical)
}
