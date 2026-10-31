//! Fresh-process Linux implementation; never called from a native App thread.
use super::*;
use anyhow::Context;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};
pub fn sealed_spec_fd(spec: &ExecutionSpec) -> Result<OwnedFd> {
    spec.validate()?;
    let bytes = crate::protocol::canonical_json(spec)?;
    sealed_bytes_fd(&bytes)
}
pub(crate) fn sealed_bytes_fd(bytes: &[u8]) -> Result<OwnedFd> {
    ensure!(
        bytes.len() <= 1024 * 1024,
        "execution specification exceeds1MiB"
    );
    let raw = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"rudder-execution-spec".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(raw >= 0, "sealed execution memory is unavailable");
    let mut file = unsafe { File::from_raw_fd(raw as i32) };
    file.write_all(bytes)?;
    file.seek(SeekFrom::Start(0))?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } == 0,
        "cannot seal execution specification"
    );
    Ok(file.into())
}
pub fn read_spec_fd(fd: OwnedFd) -> Result<ExecutionSpec> {
    let bytes = read_sealed_bytes(fd)?;
    let spec: ExecutionSpec =
        serde_json::from_slice(&bytes).context("invalid sealed execution specification")?;
    spec.validate()?;
    Ok(spec)
}
pub(crate) fn read_sealed_bytes(fd: OwnedFd) -> Result<Vec<u8>> {
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        seals >= 0 && seals & required == required,
        "execution specification FD is not fully sealed"
    );
    let mut file = File::from(fd);
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= 1024 * 1024,
        "invalid execution specification FD size/type"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "execution specification exceeds1MiB"
    );
    Ok(bytes)
}
pub fn run_helper(_spec: ExecutionSpec) -> Result<ExecutionOutcome> {
    supervisor(_spec, None, None)
}
pub fn run_helper_with_services(
    spec: ExecutionSpec,
    factory: HelperServicesFactory,
) -> Result<ExecutionOutcome> {
    supervisor(spec, Some(factory), None)
}
pub fn run_helper_with_managed_services(
    spec: ExecutionSpec,
    factory: HelperServicesFactory,
    control: super::handoff::ManagedControl,
) -> Result<ExecutionOutcome> {
    control.binding.validate(&spec)?;
    supervisor(spec, Some(factory), Some(control))
}

use super::{
    root::Layout,
    state::{self, Preflight},
};
use crate::vfs::{
    linux::{mount, MountedVfs, PrivateMountNamespace},
    partial_rpc::{
        PartialAttemptAuthority, PartialBinding, PartialRpc, PartialRpcConfig, PartialRpcLimits,
        PartialRpcSettled,
    },
    rpc::{ObservationRpc, RpcConfig, RpcLimits},
    VfsView,
};
use std::{
    fs,
    os::{
        fd::RawFd,
        unix::{
            net::UnixStream,
            process::{CommandExt, ExitStatusExt},
        },
    },
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const AUTHORIZATION_POLL: Duration = Duration::from_millis(100);
const AUTHORIZATION_SETTLE: Duration = Duration::from_secs(3);

struct LiveAuthority {
    source: Arc<dyn AttemptAuthority>,
    binding: ObservationBinding,
    revoked: AtomicBool,
}
impl AttemptAuthority for LiveAuthority {
    fn validate(&self, binding: &ObservationBinding) -> Result<()> {
        ensure!(
            binding == &self.binding && !self.cancelled(),
            "workspace attempt is no longer authorized"
        );
        if self.source.validate(binding).is_err() {
            self.revoked.store(true, Ordering::Release);
            anyhow::bail!("workspace attempt authorization failed or became unavailable");
        }
        ensure!(
            !self.cancelled(),
            "workspace attempt was revoked during authentication"
        );
        Ok(())
    }
    fn cancelled(&self) -> bool {
        self.revoked.load(Ordering::Acquire) || self.source.cancelled()
    }
}
struct AuthorityMonitor {
    authority: Arc<LiveAuthority>,
    stop: Arc<AtomicBool>,
    done: std::sync::mpsc::Receiver<()>,
}
impl AuthorityMonitor {
    fn start(source: Arc<dyn AttemptAuthority>, binding: ObservationBinding) -> Result<Self> {
        let authority = Arc::new(LiveAuthority {
            source,
            binding,
            revoked: AtomicBool::new(false),
        });
        authority.validate(&authority.binding)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (send, done) = std::sync::mpsc::sync_channel(1);
        let (running, stopped) = (authority.clone(), stop.clone());
        std::thread::Builder::new()
            .name("rudder-workspace-authority".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    if running.source.cancelled()
                        || running.source.validate(&running.binding).is_err()
                    {
                        running.revoked.store(true, Ordering::Release);
                        break;
                    }
                    let deadline = Instant::now() + AUTHORIZATION_POLL;
                    while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                let _ = send.send(());
            })?;
        Ok(Self {
            authority,
            stop,
            done,
        })
    }
    fn check(&self) -> Result<()> {
        ensure!(
            !self.authority.cancelled(),
            "workspace authority retired; execution cannot acknowledge a checkpoint"
        );
        Ok(())
    }
    fn validate_current(&self) -> Result<()> {
        self.authority.validate(&self.authority.binding)
    }
    fn finish(&self) -> Result<()> {
        self.authority.validate(&self.authority.binding)?;
        self.stop.store(true, Ordering::Release);
        self.done
            .recv_timeout(AUTHORIZATION_SETTLE)
            .context("workspace authority monitor did not settle")?;
        self.authority.validate(&self.authority.binding)
    }
}
impl Drop for AuthorityMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct ExecutionMount {
    mounted: Option<MountedVfs>,
    rpc: Option<WorkspaceRpc>,
    application: Option<ApplicationLifecycle>,
}
enum WorkspaceRpc {
    Complete(ObservationRpc),
    Partial {
        rpc: PartialRpc,
        settled: std::sync::Mutex<Option<PartialRpcSettled>>,
    },
}
impl WorkspaceRpc {
    fn close(&self) -> Result<()> {
        match self {
            Self::Complete(rpc) => rpc.await_closed(Duration::from_secs(35)),
            Self::Partial { rpc, settled } => {
                let mut receipt = settled
                    .lock()
                    .map_err(|_| anyhow::anyhow!("partial settlement ownership unavailable"))?;
                if receipt.is_none() {
                    *receipt = Some(rpc.await_closed(Duration::from_secs(35))?);
                }
                ensure!(
                    receipt.as_ref().unwrap().binding_digest() == rpc.binding().digest()?,
                    "partial settlement changed binding"
                );
                Ok(())
            }
        }
    }
    fn partial_settlement(&self) -> Result<(String, String)> {
        let Self::Partial { rpc, settled } = self else {
            anyhow::bail!("remote drain has no partial RPC owner")
        };
        let receipt = settled
            .lock()
            .map_err(|_| anyhow::anyhow!("partial settlement ownership unavailable"))?;
        let receipt = receipt
            .as_ref()
            .context("partial endpoint has not actually settled")?;
        ensure!(
            receipt.binding_digest() == rpc.binding().digest()?,
            "partial endpoint settlement differs"
        );
        Ok((
            receipt.binding_digest().into(),
            receipt.endpoint_instance().into(),
        ))
    }
    fn complete(&self) -> Result<&ObservationRpc> {
        match self {
            Self::Complete(rpc) => Ok(rpc),
            _ => anyhow::bail!("complete application cannot use partial RPC"),
        }
    }
}

struct LivePartialAuthority {
    source: Arc<dyn PartialAttemptAuthority>,
    binding: PartialBinding,
    revoked: AtomicBool,
}
impl PartialAttemptAuthority for LivePartialAuthority {
    fn validate(&self, binding: &PartialBinding) -> Result<()> {
        ensure!(
            binding == &self.binding && !self.cancelled(),
            "partial attempt retired"
        );
        if self.source.validate(binding).is_err() {
            self.revoked.store(true, Ordering::Release);
            anyhow::bail!("partial attempt unavailable or revoked");
        }
        ensure!(
            !self.cancelled(),
            "partial attempt retired during authentication"
        );
        Ok(())
    }
    fn cancelled(&self) -> bool {
        self.revoked.load(Ordering::Acquire) || self.source.cancelled()
    }
}
struct PartialAuthorityMonitor {
    authority: Arc<LivePartialAuthority>,
    stop: Arc<AtomicBool>,
    done: std::sync::mpsc::Receiver<()>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl PartialAuthorityMonitor {
    fn start(source: Arc<dyn PartialAttemptAuthority>, binding: PartialBinding) -> Result<Self> {
        let authority = Arc::new(LivePartialAuthority {
            source,
            binding,
            revoked: AtomicBool::new(false),
        });
        authority.validate(&authority.binding)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (send, done) = std::sync::mpsc::sync_channel(1);
        let (running, stopped) = (authority.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("rudder-partial-authority".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    if running.validate(&running.binding).is_err() {
                        break;
                    }
                    let deadline = Instant::now() + AUTHORIZATION_POLL;
                    while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                let _ = send.send(());
            })?;
        Ok(Self {
            authority,
            stop,
            done,
            thread: std::sync::Mutex::new(Some(thread)),
        })
    }
    fn check(&self) -> Result<()> {
        ensure!(!self.authority.cancelled(), "partial authority retired");
        Ok(())
    }
    fn validate_current(&self) -> Result<()> {
        self.authority.validate(&self.authority.binding)
    }
    fn finish(&self) -> Result<()> {
        self.validate_current()?;
        self.stop.store(true, Ordering::Release);
        self.done
            .recv_timeout(AUTHORIZATION_SETTLE)
            .context("partial authority monitor has not settled")?;
        if let Some(thread) = self
            .thread
            .lock()
            .map_err(|_| anyhow::anyhow!("partial monitor ownership unavailable"))?
            .take()
        {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("partial authority monitor failed"))?;
        }
        self.validate_current()
    }
}
impl Drop for PartialAuthorityMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
enum WorkspaceMonitor {
    Complete(AuthorityMonitor),
    Partial(PartialAuthorityMonitor),
}
impl WorkspaceMonitor {
    fn check(&self) -> Result<()> {
        match self {
            Self::Complete(m) => m.check(),
            Self::Partial(m) => m.check(),
        }
    }
    fn validate_current(&self) -> Result<()> {
        match self {
            Self::Complete(m) => m.validate_current(),
            Self::Partial(m) => m.validate_current(),
        }
    }
    fn finish(&self) -> Result<()> {
        match self {
            Self::Complete(m) => m.finish(),
            Self::Partial(m) => m.finish(),
        }
    }
}
struct ApplicationLifecycle {
    service: Box<dyn HelperApplication>,
    settlement_attempted: bool,
    guard: Option<Box<dyn HelperFinalizationGuard>>,
}
impl ApplicationLifecycle {
    fn new(service: Box<dyn HelperApplication>) -> Self {
        Self {
            service,
            settlement_attempted: false,
            guard: None,
        }
    }
    fn quiesce(&mut self) -> Result<()> {
        self.service.close_ingress();
        if self.guard.is_some() {
            return Ok(());
        }
        ensure!(
            !self.settlement_attempted,
            "application settlement previously failed; finalization remains fenced"
        );
        self.settlement_attempted = true;
        self.guard = Some(self.service.quiesce()?);
        Ok(())
    }
}
impl ExecutionMount {
    fn close_application(&mut self) -> Result<()> {
        if let Some(application) = &mut self.application {
            application.quiesce()?;
        }
        Ok(())
    }
    fn close_rpc(&self) -> Result<()> {
        if let Some(rpc) = &self.rpc {
            rpc.close()?;
        }
        Ok(())
    }
    fn unmount(&mut self) -> Result<VfsView> {
        self.close_application()?;
        self.close_rpc()?;
        self.mounted
            .as_mut()
            .context("execution mount absent")?
            .unmount()
    }
}
impl Drop for ExecutionMount {
    fn drop(&mut self) {
        if self.close_application().is_err() || self.close_rpc().is_err() {
            // Never run a controlled unmount while an observation effect still
            // owns the view. Leave it fenced until this fresh helper exits;
            // kernel/process cleanup is a failure, never checkpoint proof.
            if let Some(mounted) = self.mounted.take() {
                std::mem::forget(mounted);
            }
        }
    }
}

/// Actual owned wrapper and application/RPC/FUSE drain, constructed only after
/// kernel reap and successful unmount. Saved drain JSON cannot recreate it.
pub(super) struct OwnedExecutionDrain {
    _wrapper: Child,
    _mount: ExecutionMount,
    execution_id: String,
    operation_id: String,
    view_id: String,
    owner: crate::protocol::OwnerEpoch,
    request_digest: String,
    agent_exit: i32,
    stop: Option<super::StopAndCheckpoint>,
    network: Option<super::network_phase::OwnedNetworkReady>,
    workspace_view: VfsView,
    frozen: Option<crate::vfs::FrozenRemoteView>,
}
impl OwnedExecutionDrain {
    pub(super) fn authenticated_stop(&self) -> Result<&super::StopAndCheckpoint> {
        self.stop
            .as_ref()
            .context("remote drain has no authenticated stop")
    }
    pub(super) fn agent_exit(&self) -> i32 {
        self.agent_exit
    }
    pub(super) fn validate_remote_finalization(
        &self,
        record: &ExecutionRecord,
        workspace: &OwnedHelperWorkspace,
    ) -> Result<Vec<u8>> {
        use sha2::{Digest, Sha256};
        ensure!(
            record.format_version() == 2
                && record.phase() == ExecutionPhase::Running
                && record.execution_id() == self.execution_id
                && record.operation_id() == self.operation_id
                && record.view_id() == self.view_id
                && record.owner() == &self.owner
                && record.request_digest() == self.request_digest
                && record.supervisor_pid() == std::process::id(),
            "remote drain differs from actual running execution"
        );
        let request = workspace
            .remote_request()
            .context("remote drain requires a remote workspace")?;
        let stop = self.authenticated_stop()?;
        ensure!(
            stop.execution_id == self.execution_id
                && stop.operation_id == self.operation_id
                && stop.view_id == self.view_id
                && stop.owner == self.owner
                && stop.run_id == request.attempt.run_id
                && stop.run_epoch == request.attempt.run_epoch
                && request.observation.view_id == self.view_id
                && request.attempt.owner == self.owner
                && workspace.view().descriptor()?.remote_finalization_format == Some(1),
            "remote drain assignment differs"
        );
        let (partial_binding_digest, endpoint_instance) = self
            ._mount
            .rpc
            .as_ref()
            .context("remote drain lacks RPC owner")?
            .partial_settlement()?;
        let network = self
            .network
            .as_ref()
            .context("remote drain lacks actual network readiness")?;
        ensure!(
            network.frame().execution_request_digest == self.request_digest,
            "remote network drain differs from request"
        );
        let audit = serde_json::json!({ "format_version": 1, "execution_id": self.execution_id, "operation_id": self.operation_id,
            "view_id": self.view_id, "owner": self.owner, "execution_request_digest": self.request_digest,
            "launch_request_digest": hex::encode(Sha256::digest(crate::protocol::canonical_json(request)?)),
            "agent_exit": self.agent_exit, "stop": stop, "partial_binding_digest": partial_binding_digest,
            "endpoint_instance": endpoint_instance, "network": network.audit()?, "kernel_drained": true });
        let bytes = crate::protocol::canonical_json(&audit)?;
        ensure!(bytes.len() <= 32 * 1024, "remote drain audit exceeds bound");
        Ok(bytes)
    }
    pub(super) fn validate_commit(
        &self,
        committed: &super::CommittedExecutionRecord,
    ) -> Result<()> {
        let record = committed.record();
        ensure!(
            record.execution_id() == self.execution_id
                && record.operation_id() == self.operation_id
                && record.view_id() == self.view_id
                && record.owner() == &self.owner
                && record.request_digest() == self.request_digest
                && record.agent_exit() == Some(self.agent_exit)
                && record.phase() == ExecutionPhase::Checkpointed
                && record.supervisor_pid() == std::process::id(),
            "committed execution differs from the actual owned drain"
        );
        if let Some(network) = &self.network {
            network.validate_retained()?;
        }
        if matches!(record.checkpoint(), Some(CheckpointRef::PartialRemote(_))) {
            self._mount
                .rpc
                .as_ref()
                .context("partial commit lacks actual RPC settlement")?
                .partial_settlement()?;
            ensure!(
                self.network.is_some() && self.stop.is_some(),
                "partial commit lacks actual network/stop drain"
            );
        }
        Ok(())
    }
    pub(super) fn validate_stop(&self, stop: &super::StopAndCheckpoint) -> Result<()> {
        ensure!(
            self.stop.as_ref() == Some(stop),
            "drain has no matching authenticated stop"
        );
        Ok(())
    }
    pub(super) fn validate_publication(
        &self,
        committed: &super::CommittedExecutionRecord,
    ) -> Result<()> {
        self.validate_commit(committed)?;
        if let Some(CheckpointRef::PartialRemote(checkpoint)) = committed.record().checkpoint() {
            let frozen = self
                .frozen
                .as_ref()
                .context("partial publication lacks its actual frozen guard")?;
            frozen.check_for(&self.workspace_view)?;
            ensure!(
                frozen.intent_reference() == checkpoint.freeze_intent
                    && frozen.intent().request.capture.nonce == checkpoint.capture.request_nonce,
                "partial publication differs from the owned frozen intent"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod application_lifecycle_tests {
    use super::*;
    use fs2::FileExt;
    use std::sync::atomic::AtomicUsize;

    struct Fence {
        _file: File,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for Fence {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    struct Service {
        file: Option<File>,
        attempts: Arc<AtomicUsize>,
        closed: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        fail: bool,
    }
    impl HelperApplication for Service {
        fn start(&mut self, _ready: &WorkspaceRpcReady<'_>) -> Result<GuestServiceProjection> {
            anyhow::bail!("unused without a real owning RPC")
        }
        fn close_ingress(&self) {
            self.closed.store(true, Ordering::Release);
        }
        fn quiesce(&mut self) -> Result<Box<dyn HelperFinalizationGuard>> {
            assert!(self.closed.load(Ordering::Acquire));
            self.attempts.fetch_add(1, Ordering::AcqRel);
            ensure!(!self.fail, "deliberate unsettled application");
            let file = self.file.take().unwrap();
            file.try_lock_exclusive()?;
            Ok(Box::new(Fence {
                _file: file,
                dropped: self.dropped.clone(),
            }))
        }
    }
    #[test]
    fn finalization_owns_real_writer_fence_until_after_durable_record() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("writer.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let service = Service {
            file: Some(file),
            attempts: attempts.clone(),
            closed: closed.clone(),
            dropped: dropped.clone(),
            fail: false,
        };
        let mut mounted = ExecutionMount {
            mounted: None,
            rpc: None,
            application: Some(ApplicationLifecycle::new(Box::new(service))),
        };
        mounted.close_application().unwrap();
        assert!(closed.load(Ordering::Acquire));
        let observer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(observer.try_lock_exclusive().is_err());
        let record = root.path().join("execution-record.json");
        fs::write(&record, b"{\"phase\":\"checkpointed\"}").unwrap();
        File::open(&record).unwrap().sync_all().unwrap();
        File::open(root.path()).unwrap().sync_all().unwrap();
        assert!(!dropped.load(Ordering::Acquire));
        assert!(observer.try_lock_exclusive().is_err());
        drop(mounted);
        assert!(dropped.load(Ordering::Acquire));
        observer.try_lock_exclusive().unwrap();
        assert_eq!(attempts.load(Ordering::Acquire), 1);
    }
    #[test]
    fn failed_settlement_is_not_retried_or_promoted_to_success_on_drop() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let service = Service {
            file: None,
            attempts: attempts.clone(),
            closed,
            dropped: dropped.clone(),
            fail: true,
        };
        let mut mounted = ExecutionMount {
            mounted: None,
            rpc: None,
            application: Some(ApplicationLifecycle::new(Box::new(service))),
        };
        assert!(mounted.close_application().is_err());
        assert!(mounted.close_application().is_err());
        drop(mounted);
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        assert!(!dropped.load(Ordering::Acquire));
    }
}

#[derive(Serialize, Deserialize)]
struct ChildSpec {
    spec: ExecutionSpec,
    layout: Layout,
    nonce: String,
    execution_request_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    application_projection: Option<GuestServiceProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managed: Option<ManagedExecutionBinding>,
}
#[derive(Serialize, Deserialize)]
struct InitReport {
    nonce: String,
    agent_exit: i32,
    started: bool,
    error: Option<String>,
    #[serde(default)]
    managed_stop: bool,
}
#[derive(Serialize, Deserialize)]
struct DrainReport {
    nonce: String,
    agent_exit: i32,
    kernel_drained: bool,
    #[serde(default)]
    managed_stop: bool,
}

pub fn arm_parent(parent: u32) -> Result<OwnedFd> {
    ensure!(
        parent > 1 && unsafe { libc::getppid() } == parent as i32,
        "execution parent is no longer its owner"
    );
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, parent, 0) };
    ensure!(fd >= 0, "execution parent lifetime descriptor unavailable");
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
        "cannot fence execution parent death"
    );
    check_lifetime(fd.as_raw_fd())?;
    Ok(fd)
}
fn check_lifetime(fd: RawFd) -> Result<()> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut p, 1, 0) } == 0,
        "execution parent died during initialization"
    );
    Ok(())
}
fn proof() -> ExecutionIsolation {
    ExecutionIsolation {
        same_uid_user_namespace: true,
        private_mount_namespace: true,
        kernel_pid_namespace_drained: true,
        restricted_root: true,
        read_only_runtime: true,
        inherited_authority_fds_closed: true,
        no_new_privileges: true,
        network_isolated: false,
        cgroup_limited: false,
    }
}
fn supervisor(
    spec: ExecutionSpec,
    factory: Option<HelperServicesFactory>,
    mut control: Option<super::handoff::ManagedControl>,
) -> Result<ExecutionOutcome> {
    spec.validate()?;
    if spec.format_version == 4 {
        ensure!(
            control
                .as_ref()
                .is_some_and(|control| control.binding.format_version == 3),
            "v4 execution requires its owned managed shutdown controller"
        );
    }
    ensure!(
        unsafe { libc::geteuid() } == 1500 && unsafe { libc::getegid() } == 1500,
        "execution profile requires worker UID/GID1500"
    );
    ensure!(
        fs::read_dir("/proc/self/task")?.count() == 1,
        "execution supervisor must be a fresh helper"
    );
    let parent = unsafe { libc::getppid() } as u32;
    let lifetime = arm_parent(parent)?;
    let namespace = PrivateMountNamespace::enter_for_current_user()?;
    check_lifetime(lifetime.as_raw_fd())?;
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0,
        "cannot protect supervisor descriptors"
    );
    // Hold and reconcile actual execution state before any services/workspace
    // factory starts authority, source/cache, VFS or application effects.
    let (mut state, mut services) = match state::preflight(&spec, factory)? {
        Preflight::Recovered(state) => {
            return Ok(ExecutionOutcome::Historical(state.into_historical()))
        }
        Preflight::New { state, services } => (state, services),
    };
    let result = (|| -> Result<ExecutionOutcome> {
        let expected_application = spec
            .workspace_observation
            .as_ref()
            .and_then(|w| w.application.as_ref());
        ensure!(
            expected_application.is_some()
                == services.as_ref().is_some_and(|s| s.application.is_some()),
            "application specification has no matching trusted factory"
        );
        let partial_binding = spec
            .profile
            .as_ref()
            .map(|profile| profile.partial_binding())
            .transpose()?
            .flatten();
        let monitor = match (&spec.workspace_observation, &services) {
            (Some(workspace), Some(services)) => Some(match &services.workspace {
                HelperWorkspaceServices::Complete { authority, .. } => {
                    ensure!(
                        partial_binding.is_none(),
                        "remote selection cannot use complete authority"
                    );
                    WorkspaceMonitor::Complete(AuthorityMonitor::start(
                        authority.clone(),
                        workspace.binding.clone(),
                    )?)
                }
                HelperWorkspaceServices::Partial { authority } => {
                    WorkspaceMonitor::Partial(PartialAuthorityMonitor::start(
                        authority.clone(),
                        partial_binding
                            .clone()
                            .context("partial authority requires a sealed remote selection")?,
                    )?)
                }
            }),
            (None, None) => None,
            _ => anyhow::bail!("workspace helper service/profile mismatch"),
        };
        unsafe {
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        }
        let workspace = state.open_workspace(
            &spec,
            services
                .as_mut()
                .and_then(|services| services.workspace_factory.take()),
        )?;
        if partial_binding.is_some() {
            ensure!(
                workspace.checkpoint_capability() == CheckpointCapability::PartialRemoteFinalizable,
                "remote execution requires actual namespace5 finalization"
            );
        } else {
            workspace.require_supported_execution()?;
        }
        // Keep this owner outside the guest namespaces through mount/application
        // drain, retained checkpoint, record fsync and authenticated receipt.
        let manager = workspace.manager();
        let view = workspace.view();
        let mountpoint = manager.root().join("mounts").join(&spec.view_id);
        let layout = Layout {
            root: state.directory.join("root"),
            home: state.directory.join("home"),
            tmp: state.directory.join("tmp"),
            scratch: state.directory.join("scratch"),
            view: mountpoint.clone(),
        };
        for path in [&layout.root, &layout.home, &layout.tmp, &layout.scratch] {
            super::root::private_dir(path)?;
        }
        super::root::seed_home(&layout, &spec)?;
        let lease = manager.store().begin_staging(Duration::from_secs(3600))?;
        let result = (|| -> Result<ExecutionOutcome> {
            let mut mounted = ExecutionMount {
                mounted: Some(mount(view.clone(), &mountpoint, &namespace)?),
                rpc: None,
                application: services
                    .as_mut()
                    .and_then(|s| s.application.take())
                    .map(ApplicationLifecycle::new),
            };
            if let Some(workspace) = &spec.workspace_observation {
                let services = services.as_ref().context("workspace services absent")?;
                match (
                    &services.workspace,
                    monitor.as_ref().context("workspace monitor absent")?,
                ) {
                    (
                        HelperWorkspaceServices::Complete {
                            adoption_witness, ..
                        },
                        WorkspaceMonitor::Complete(monitor),
                    ) => {
                        let rpc = ObservationRpc::bind(
                            view.clone(),
                            RpcConfig {
                                attempt: workspace.binding.attempt.clone(),
                                profile: workspace.binding.profile.clone(),
                                authority: monitor.authority.clone(),
                                adoption_witness: adoption_witness.clone(),
                                limits: RpcLimits::default(),
                            },
                            &workspace.endpoint_directory,
                        )?;
                        ensure!(
                            rpc.binding() == &workspace.binding,
                            "mounted workspace differs from sealed observation"
                        );
                        super::root::validate_workspace_projection(workspace)?;
                        mounted.rpc = Some(WorkspaceRpc::Complete(rpc));
                    }
                    (
                        HelperWorkspaceServices::Partial { .. },
                        WorkspaceMonitor::Partial(monitor),
                    ) => {
                        let rpc = PartialRpc::bind(
                            view.clone(),
                            PartialRpcConfig {
                                authority: monitor.authority.clone(),
                                limits: PartialRpcLimits::default(),
                            },
                            &workspace.endpoint_directory,
                        )?;
                        ensure!(
                            Some(rpc.binding()) == partial_binding.as_ref(),
                            "partial endpoint differs from sealed selection"
                        );
                        super::root::validate_partial_workspace_projection(
                            workspace,
                            rpc.binding(),
                        )?;
                        mounted.rpc = Some(WorkspaceRpc::Partial {
                            rpc,
                            settled: std::sync::Mutex::new(None),
                        });
                    }
                    _ => anyhow::bail!("workspace authority family differs from the owned monitor"),
                }
            }
            let application_projection = if let Some(application) = &mut mounted.application {
                let workspace = spec
                    .workspace_observation
                    .as_ref()
                    .context("application workspace absent")?;
                let rpc = mounted
                    .rpc
                    .as_ref()
                    .context("application requires the owning workspace RPC")?
                    .complete()?;
                let projection = application.service.start(&WorkspaceRpcReady {
                    binding: rpc.binding(),
                    socket: rpc.socket_path(),
                    identity_file: &workspace.endpoint_directory.join("identity.json"),
                    application: workspace
                        .application
                        .as_ref()
                        .context("application assignment absent")?,
                })?;
                super::root::validate_application_projection(&spec, &projection)?;
                if let Some(monitor) = &monitor {
                    monitor.check()?;
                }
                Some(projection)
            } else {
                None
            };
            let nonce = Uuid::new_v4().to_string();
            let mut projected = spec.clone();
            projected.home_files.clear();
            let childspec = ChildSpec {
                spec: projected,
                layout,
                nonce: nonce.clone(),
                execution_request_digest: state.record().request_digest().to_owned(),
                application_projection,
                managed: control.as_ref().map(|c| c.binding.clone()),
            };
            let payload = sealed_payload(&childspec)?;
            let (mut receive, send) = UnixStream::pair()?;
            let (mut supervisor_control, init_control) = UnixStream::pair()?;
            supervisor_control.set_read_timeout(Some(Duration::from_secs(2)))?;
            supervisor_control.set_write_timeout(Some(Duration::from_secs(2)))?;
            receive.set_read_timeout(Some(Duration::from_secs(3)))?;
            let fd3 = duplicate_high(payload.as_raw_fd())?;
            let fd4 = duplicate_high(send.as_raw_fd())?;
            let fd5 = (control.is_some() || spec.profile.is_some())
                .then(|| duplicate_high(init_control.as_raw_fd()))
                .transpose()?;
            let mut command = Command::new("/proc/self/exe");
            command
                .args(["fabric", "execution", "child"])
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd3.as_raw_fd(), 3) < 0
                        || libc::dup2(fd4.as_raw_fd(), 4) < 0
                        || fd5
                            .as_ref()
                            .is_some_and(|fd| libc::dup2(fd.as_raw_fd(), 5) < 0)
                        || libc::setpgid(0, 0) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::syscall(
                        libc::SYS_close_range,
                        if fd5.is_some() { 6u32 } else { 5u32 },
                        u32::MAX,
                        4u32,
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            state.mark_running()?;
            let mut child = command
                .spawn()
                .context("spawn restricted execution init wrapper")?;
            drop(command);
            drop(send);
            drop(payload);
            drop(init_control);
            let mut guard = ProcessGuard(child.id() as i32);
            let network_ready = if spec.profile.is_some() {
                supervisor_control.set_read_timeout(Some(Duration::from_secs(10)))?;
                let (frame, descriptors): (super::network_phase::NetworkReadyFrame, _) =
                    super::handoff::receive_control_packet_fds(&mut supervisor_control)?;
                let process = super::handoff::pidfd(child.id())?;
                let witness = super::network_phase::OwnedNetworkReady::receive(
                    &spec,
                    &nonce,
                    frame,
                    &process,
                    child.id(),
                    descriptors,
                )?;
                supervisor_control.write_all(b"N")?;
                supervisor_control.set_read_timeout(Some(Duration::from_secs(2)))?;
                Some(witness)
            } else {
                None
            };
            let began = Instant::now();
            let mut renewed = Instant::now();
            let mut stop_sent = false;
            let mut backend_ready = false;
            let mut authenticated_sent = false;
            let status = loop {
                if let Some(monitor) = &monitor {
                    monitor.check()?;
                }
                if let Some(control) = &mut control {
                    if control.poll_stop()? && !stop_sent {
                        supervisor_control.write_all(b"S")?;
                        stop_sent = true;
                    }
                    if control.authenticated && !authenticated_sent && !stop_sent {
                        supervisor_control.write_all(b"A")?;
                        authenticated_sent = true;
                    }
                    let mut poll = libc::pollfd {
                        fd: supervisor_control.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut poll, 1, 0) } > 0
                        && poll.revents & libc::POLLIN != 0
                        && !backend_ready
                    {
                        let (execution, descriptor): (String, _) =
                            super::handoff::receive_control_packet(&mut supervisor_control)?;
                        ensure!(
                            execution == spec.execution_id,
                            "backend readiness belongs to another execution"
                        );
                        let process = descriptor.context("backend readiness lacks actual pidfd")?;
                        if let Some(network) = &network_ready {
                            control.backend_ready_network(&process, network)?;
                        } else {
                            control.backend_ready(&process)?;
                        }
                        backend_ready = true;
                    }
                }
                if let Some(status) = child.try_wait()? {
                    guard.0 = 0;
                    break status;
                }
                if began.elapsed() > Duration::from_secs(spec.timeout_seconds + 30) {
                    let _ = child.kill();
                    let _ = child.wait();
                    guard.0 = 0;
                    anyhow::bail!("restricted execution drain deadline exceeded")
                }
                if renewed.elapsed() > Duration::from_secs(60) {
                    manager
                        .store()
                        .renew_staging(&lease.id, Duration::from_secs(3600))?;
                    renewed = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            ensure!(
                status.success(),
                "execution init wrapper failed; no checkpoint can be acknowledged"
            );
            let mut bytes = Vec::new();
            (&mut receive).take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 64 * 1024,
                "execution drain report exceeds limit"
            );
            let drain: DrainReport = serde_json::from_slice(&bytes)
                .context("missing execution namespace-drain proof")?;
            ensure!(
                drain.nonce == nonce && drain.kernel_drained,
                "execution namespace-drain proof mismatch"
            );
            if let Some(control) = &control {
                ensure!(
                    drain.managed_stop && control.request.is_some(),
                    "kernel shutdown has no matching authenticated parent request"
                );
            }
            if unsafe { libc::isatty(0) } == 1 {
                unsafe {
                    libc::tcsetpgrp(0, libc::getpgrp());
                }
            }
            // The application keeps its owned finalization guard in `mounted`
            // through RPC/FUSE drain, retained checkpoint and record fsync.
            mounted.close_application()?;
            mounted.close_rpc()?;
            if let Some(monitor) = &monitor {
                monitor.check()?;
            }
            let _view = mounted.unmount()?;
            let mut owned_drain = OwnedExecutionDrain {
                _wrapper: child,
                _mount: mounted,
                execution_id: spec.execution_id.clone(),
                operation_id: spec.operation_id.clone(),
                view_id: spec.view_id.clone(),
                owner: spec.owner.clone(),
                request_digest: state.record().request_digest().to_owned(),
                agent_exit: drain.agent_exit,
                stop: control.as_ref().and_then(|value| value.request.clone()),
                network: network_ready,
                workspace_view: view.clone(),
                frozen: None,
            };
            if let Some(monitor) = &monitor {
                monitor.validate_current()?;
            }
            let committed = if partial_binding.is_some() {
                owned_drain.validate_remote_finalization(state.record(), &workspace)?;
                let request = crate::vfs::RemoteFreezeRequest {
                    format_version: 1,
                    intent_id: Uuid::new_v4().to_string(),
                    capture: crate::vfs::PartialCaptureRequest {
                        nonce: owned_drain.authenticated_stop()?.request_id.clone(),
                        expected_begin: view.dirty_cursor()?,
                        expected_head: view.partial_capture_head()?.map(|head| head.request_nonce),
                    },
                };
                let mut frozen = view.freeze_remote(&request, Duration::from_secs(35))?;
                let committed = state.finalize_remote(
                    &workspace,
                    &owned_drain,
                    &mut frozen,
                    crate::vfs::PartialCaptureBudget::default(),
                )?;
                frozen.check_for(view)?;
                owned_drain.frozen = Some(frozen);
                committed
            } else {
                state.finalize_local(&workspace, drain.agent_exit)?
            };
            if let Some(monitor) = &monitor {
                monitor.validate_current()?;
            }
            if let Some(monitor) = &monitor {
                monitor.finish()?;
            }
            state.verify_commit_current(&committed)?;
            owned_drain.validate_publication(&committed)?;
            if let Some(control) = &mut control {
                control.shutdown_receipt(&committed, &owned_drain)?;
            }
            let mut isolation = proof();
            isolation.network_isolated = owned_drain.network.is_some();
            ExecutionOutcome::from_drained(committed, isolation, Vec::new(), owned_drain)
        })();
        let release = manager.store().abandon_staging(&lease.id);
        match result {
            Ok(mut value) => {
                if release.is_err() {
                    value
                        .warnings_mut()
                        .context("new execution returned historical output")?
                        .push(
                            "checkpoint committed; execution staging cleanup needs reconciliation"
                                .into(),
                        );
                }
                Ok(value)
            }
            Err(e) => Err(e),
        }
    })();
    if result.is_err() && state.record().phase() != ExecutionPhase::Checkpointed {
        let _ = state.mark_orphaned();
    }
    result
}

fn duplicate_high(fd: RawFd) -> Result<OwnedFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}
fn sealed_payload<T: Serialize>(value: &T) -> Result<OwnedFd> {
    let bytes = crate::protocol::canonical_json(value)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "execution child payload exceeds1MiB"
    );
    let raw = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"rudder-execution-child".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(raw >= 0, "execution child memfd unavailable");
    let mut file = unsafe { File::from_raw_fd(raw as i32) };
    file.write_all(&bytes)?;
    file.seek(SeekFrom::Start(0))?;
    ensure!(
        unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        } == 0,
        "cannot seal child payload"
    );
    Ok(file.into())
}
fn read_payload(fd: OwnedFd) -> Result<ChildSpec> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        flags >= 0 && flags & required == required,
        "child payload FD is not sealed"
    );
    let mut file = File::from(fd);
    ensure!(
        file.metadata()?.len() <= 1024 * 1024,
        "child payload too large"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "child payload exceeds1MiB");
    Ok(serde_json::from_slice(&bytes)?)
}

/// Internal fresh-exec entry; only sealed FD3 and private report FD4 survive.
pub fn run_child() -> Result<()> {
    ensure!(
        fs::read_dir("/proc/self/task")?.count() == 1,
        "execution child wrapper must be fresh"
    );
    let payload = read_payload(unsafe { OwnedFd::from_raw_fd(3) })?;
    payload.spec.validate()?;
    if let Some(binding) = &payload.managed {
        binding.validate(&payload.spec)?;
    }
    let mut init_control = (payload.managed.is_some() || payload.spec.profile.is_some())
        .then(|| UnixStream::from(unsafe { OwnedFd::from_raw_fd(5) }));
    let mut report = unsafe { File::from_raw_fd(4) };
    let parent = unsafe { libc::getppid() } as u32;
    let lifetime = arm_parent(parent)?;
    let network_baseline = if let Some(profile) = &payload.spec.profile {
        ensure!(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } == 0,
            "cannot fence offline wrapper privileges"
        );
        Some(super::network::linux::ChildNetworkBaseline::capture(
            &profile.validated_network(&payload.spec)?,
        )?)
    } else {
        None
    };
    let _namespace = PrivateMountNamespace::enter_for_current_user()?;
    let network = network_baseline
        .map(|baseline| unsafe { baseline.enter_after_new_user() })
        .transpose()?;
    if let Some(network) = &network {
        let control = init_control
            .as_mut()
            .context("offline wrapper has no private control")?;
        control.set_read_timeout(Some(Duration::from_secs(10)))?;
        control.set_write_timeout(Some(Duration::from_secs(2)))?;
        let frame = super::network_phase::NetworkReadyFrame::from_child(
            &payload.spec,
            &payload.nonce,
            &payload.execution_request_digest,
            network,
        )?;
        let descriptors = network.child_namespace_fds().map(|fd| fd.as_raw_fd());
        super::handoff::send_control_packet_fds(control, &frame, &descriptors)?;
        let mut admission = [0u8];
        control.read_exact(&mut admission)?;
        ensure!(
            admission == *b"N",
            "parent withheld offline namespace admission"
        );
        network.verify_current()?;
    }
    check_lifetime(lifetime.as_raw_fd())?;
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0,
        "cannot protect wrapper descriptors"
    );
    ensure!(
        unsafe { libc::unshare(libc::CLONE_NEWPID) } == 0,
        "private PID namespace unavailable"
    );
    let wrapper = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
    ensure!(wrapper >= 0, "PID init lifetime descriptor unavailable");
    let wrapper = unsafe { OwnedFd::from_raw_fd(wrapper as i32) };
    let (mut init_receive, init_send) = UnixStream::pair()?;
    init_receive.set_read_timeout(Some(Duration::from_secs(2)))?;
    let init = unsafe { libc::fork() };
    ensure!(init >= 0, "cannot fork namespace init");
    if init == 0 {
        drop(report);
        drop(init_receive);
        let mut pipe = init_send;
        let result = namespace_init(
            &payload,
            wrapper.as_raw_fd(),
            init_control,
            network.as_ref(),
        );
        let code = match result {
            Ok((exit, managed_stop)) => {
                let frame = InitReport {
                    nonce: payload.nonce.clone(),
                    agent_exit: exit,
                    started: true,
                    error: None,
                    managed_stop,
                };
                if pipe
                    .write_all(&crate::protocol::canonical_json(&frame).unwrap_or_default())
                    .is_ok()
                {
                    0
                } else {
                    125
                }
            }
            Err(error) => {
                let frame = InitReport {
                    nonce: payload.nonce.clone(),
                    agent_exit: 125,
                    started: false,
                    error: Some(format!("{error:#}")),
                    managed_stop: false,
                };
                let _ =
                    pipe.write_all(&crate::protocol::canonical_json(&frame).unwrap_or_default());
                125
            }
        };
        unsafe { libc::_exit(code) }
    }
    drop(init_send);
    drop(init_control);
    let mut guard = ProcessGuard(init);
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(init, &mut status, 0) };
        if result == init {
            guard.0 = 0;
            break;
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            anyhow::bail!("cannot reap PID namespace init")
        }
    }
    let mut bytes = Vec::new();
    (&mut init_receive)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024, "init outcome too large");
    let outcome: InitReport = serde_json::from_slice(&bytes)?;
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        anyhow::bail!(
            "namespace init failed: {}",
            outcome
                .error
                .as_deref()
                .unwrap_or("unexpected process termination")
        )
    }
    ensure!(
        outcome.started && outcome.nonce == payload.nonce,
        "init outcome mismatch"
    );
    // Reaping PID1 happens only after zap_pid_ns_processes kills all remaining
    // namespace threads and waits for kernel teardown, including async I/O.
    let frame = DrainReport {
        nonce: payload.nonce,
        agent_exit: outcome.agent_exit,
        kernel_drained: true,
        managed_stop: outcome.managed_stop,
    };
    report.write_all(&crate::protocol::canonical_json(&frame)?)?;
    Ok(())
}
fn namespace_init(
    payload: &ChildSpec,
    parent: RawFd,
    mut control: Option<UnixStream>,
    network: Option<&super::network::linux::OwnedChildNetwork>,
) -> Result<(i32, bool)> {
    ensure!(
        unsafe { libc::getpid() } == 1,
        "execution is not private PID namespace init"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
        "cannot fence PID init lifetime"
    );
    check_lifetime(parent)?;
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } == 0,
        "cannot protect init descriptors"
    );
    ensure!(
        unsafe { libc::setpgid(0, 0) } == 0,
        "cannot isolate init process group"
    );
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    ensure!(
        payload
            .spec
            .workspace_observation
            .as_ref()
            .and_then(|w| w.application.as_ref())
            .is_some()
            == payload.application_projection.is_some(),
        "sealed application/guest projection mismatch"
    );
    super::root::restrict(
        &payload.layout,
        &payload.spec,
        payload.application_projection.as_ref(),
    )?;
    ensure!(
        payload.spec.profile.is_some() == network.is_some(),
        "init network ownership differs from sealed profile"
    );
    if let Some(network) = network {
        network.verify_current()?;
    }
    let mut command = Command::new(&payload.spec.command[0]);
    command
        .args(&payload.spec.command[1..])
        .env_clear()
        .envs(&payload.spec.environment)
        .env("HOME", "/home/agent")
        .env("TMPDIR", "/tmp")
        .env("PWD", "/workspace")
        .env("RUDDER_EXECUTION_SANDBOX", "1")
        .env("RUDDER_WORKSPACE_VIEW_ID", &payload.spec.view_id)
        .current_dir("/workspace")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(application) = &payload.application_projection {
        application.validate(&payload.spec)?;
        command.envs(&application.environment);
    }
    if !payload.spec.environment.contains_key("PATH") {
        command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    }
    if !payload.spec.environment.contains_key("TERM") {
        command.env("TERM", "xterm-256color");
    }
    if payload.spec.broker_directory.is_some() {
        command
            .env(
                "RUDDER_AGENT_BROKER_SOCKET",
                "/run/rudder/agent/broker.sock",
            )
            .env("RUDDER_AGENT_IDENTITY", "/run/rudder/agent/identity.json");
    }
    if let Some(workspace) = &payload.spec.workspace_observation {
        if let Some(binding) = payload
            .spec
            .profile
            .as_ref()
            .map(|profile| profile.partial_binding())
            .transpose()?
            .flatten()
        {
            command
                .env(
                    "RUDDER_PARTIAL_WORKSPACE_SOCKET",
                    "/run/rudder/workspace/partial-workspace.sock",
                )
                .env(
                    "RUDDER_PARTIAL_WORKSPACE_IDENTITY",
                    "/run/rudder/workspace/partial-identity.json",
                )
                .env(
                    "RUDDER_PARTIAL_WORKSPACE_BINDING",
                    String::from_utf8(crate::protocol::canonical_json(&binding)?)?,
                );
        } else {
            command
                .env("RUDDER_WORKSPACE_SOCKET", WORKSPACE_GUEST_SOCKET)
                .env("RUDDER_WORKSPACE_IDENTITY", WORKSPACE_GUEST_IDENTITY)
                .env(
                    "RUDDER_WORKSPACE_BINDING",
                    crate::protocol::canonical_json(&workspace.binding)
                        .map(|bytes| String::from_utf8(bytes).expect("canonical JSON is UTF-8"))?,
                );
        }
    }
    let all_five = payload.spec.profile.is_some();
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            drop_profile_privileges(all_five)?;
            Ok(())
        });
    }
    let mut server = if let Some(binding) = &payload.managed {
        if let Some(argv) = &binding.server_command {
            let mut server = Command::new(&argv[0]);
            server
                .args(&argv[1..])
                .env_clear()
                .current_dir("/workspace")
                .envs(
                    command
                        .get_envs()
                        .filter_map(|(key, value)| value.map(|value| (key, value))),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            unsafe {
                server.pre_exec(move || {
                    if libc::setpgid(0, 0) != 0
                        || libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    drop_profile_privileges(all_five)
                });
            }
            let mut child = server.spawn().context("launch owned backend server")?;
            if !wait_owned_backend(&mut child, &binding.guest_socket, control.as_mut())? {
                unsafe {
                    libc::kill(-1, libc::SIGKILL);
                }
                return Ok((143, true));
            }
            send_backend_ready(
                control.as_mut().unwrap(),
                &payload.spec.execution_id,
                child.id(),
            )?;
            ensure!(
                binding.frontend_start == ManagedFrontendStart::AfterHostApiKeyV1,
                "owned server lacks mandatory authenticated frontend admission"
            );
            if !wait_frontend_auth(&mut child, control.as_mut().unwrap())? {
                unsafe {
                    libc::kill(-1, libc::SIGKILL);
                }
                return Ok((143, true));
            }
            Some(child)
        } else {
            None
        }
    } else {
        None
    };
    let mut child = command
        .spawn()
        .context("launch command in restricted root")?;
    if let Some(binding) = &payload.managed {
        if server.is_none() {
            if binding.execution_profile != "offline-command-v1"
                && !wait_owned_backend(&mut child, &binding.guest_socket, control.as_mut())?
            {
                unsafe {
                    libc::kill(-1, libc::SIGKILL);
                }
                return Ok((143, true));
            }
            send_backend_ready(
                control.as_mut().unwrap(),
                &payload.spec.execution_id,
                child.id(),
            )?;
        }
    }
    if unsafe { libc::isatty(0) } == 1 {
        ensure!(
            unsafe { libc::tcsetpgrp(0, child.id() as i32) } == 0,
            "cannot give agent foreground PTY"
        );
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGCONT);
        }
    }
    let began = Instant::now();
    let mut managed_stop = false;
    let exit = loop {
        if let Some(control) = &mut control {
            if managed_stop_requested(control)? {
                managed_stop = true;
                break 143;
            }
        }
        if let Some(status) = child.try_wait()? {
            ensure!(payload.managed.is_none(),
                "managed backend exited without authenticated parent shutdown; checkpoint authority withheld");
            break status.code().unwrap_or(128 + status.signal().unwrap_or(9));
        }
        if began.elapsed() > Duration::from_secs(payload.spec.timeout_seconds) {
            anyhow::bail!("backend execution timed out; checkpoint authority withheld");
        }
        if let Some(server) = &mut server {
            ensure!(
                server.try_wait()?.is_none(),
                "owned backend server exited before its terminal"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // PID1's exit is the final kernel-enforced kill/reap barrier. Userland
    // attempts termination too; detached/nested namespace escape cannot evade
    // zap_pid_ns_processes when this init exits.
    unsafe {
        libc::kill(-1, libc::SIGKILL);
    }
    Ok((exit, managed_stop))
}
fn managed_stop_requested(control: &mut UnixStream) -> Result<bool> {
    let mut event = libc::pollfd {
        fd: control.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut event, 1, 0) } >= 0,
        "private init control poll failed"
    );
    if event.revents & libc::POLLIN != 0 {
        let mut byte = [0];
        control.read_exact(&mut byte)?;
        ensure!(byte == *b"S", "invalid private init stop request");
        return Ok(true);
    }
    ensure!(
        event.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0,
        "owning supervisor control ended"
    );
    Ok(false)
}
fn wait_frontend_auth(server: &mut std::process::Child, control: &mut UnixStream) -> Result<bool> {
    wait_frontend_auth_until(server, control, Instant::now() + Duration::from_secs(30))
}
fn wait_frontend_auth_until(
    server: &mut std::process::Child,
    control: &mut UnixStream,
    deadline: Instant,
) -> Result<bool> {
    loop {
        ensure!(
            Instant::now() < deadline,
            "host frontend authentication deadline ended"
        );
        ensure!(
            server.try_wait()?.is_none(),
            "owned backend exited before host authentication"
        );
        let mut event = libc::pollfd {
            fd: control.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut event, 1, 10) } >= 0,
            "private frontend admission poll failed"
        );
        if event.revents & libc::POLLIN != 0 {
            let mut byte = [0];
            control.read_exact(&mut byte)?;
            match byte[0] {
                b'A' => return Ok(true),
                b'S' => return Ok(false),
                _ => anyhow::bail!("invalid private frontend admission"),
            }
        }
        ensure!(
            event.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0,
            "parent frontend admission control ended"
        );
    }
}

#[cfg(test)]
mod frontend_auth_tests {
    use super::*;
    #[test]
    fn only_private_parent_authentication_releases_frontend_and_stop_or_eof_withholds_it() {
        for action in [b'A', b'S', b'X', 0] {
            let (mut parent, mut init) = UnixStream::pair().unwrap();
            let (result, received) = std::sync::mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                let mut server = Command::new("/bin/sleep").arg("30").spawn().unwrap();
                let admitted = wait_frontend_auth_until(
                    &mut server,
                    &mut init,
                    Instant::now() + Duration::from_secs(2),
                );
                let started = if matches!(admitted, Ok(true)) {
                    Command::new("/bin/true").status().unwrap().success()
                } else {
                    false
                };
                server.kill().unwrap();
                server.wait().unwrap();
                result.send((admitted.is_ok(), started)).unwrap();
            });
            assert!(
                received.recv_timeout(Duration::from_millis(30)).is_err(),
                "no socket/text/default readiness may start the frontend"
            );
            if action == 0 {
                drop(parent);
            } else {
                parent.write_all(&[action]).unwrap();
            }
            let (valid, started) = received.recv_timeout(Duration::from_secs(2)).unwrap();
            worker.join().unwrap();
            assert_eq!(started, action == b'A');
            assert_eq!(valid, matches!(action, b'A' | b'S'));
        }
    }
    #[test]
    fn missing_parent_authentication_is_bounded_and_never_admits_frontend() {
        let (_parent, mut init) = UnixStream::pair().unwrap();
        let mut server = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let result = wait_frontend_auth_until(
            &mut server,
            &mut init,
            Instant::now() + Duration::from_millis(40),
        );
        server.kill().unwrap();
        server.wait().unwrap();
        assert!(result.is_err());
    }
}
fn wait_owned_backend(
    child: &mut std::process::Child,
    socket: &std::path::Path,
    mut control: Option<&mut UnixStream>,
) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(stream) = control.as_deref_mut() {
            if managed_stop_requested(stream)? {
                return Ok(false);
            }
        }
        ensure!(
            child.try_wait()?.is_none(),
            "owned backend exited before readiness"
        );
        if let Ok(stream) = UnixStream::connect(socket) {
            let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
            let mut length = std::mem::size_of_val(&cred) as libc::socklen_t;
            ensure!(
                unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_PEERCRED,
                        (&mut cred as *mut libc::ucred).cast(),
                        &mut length,
                    )
                } == 0
                    && cred.pid == child.id() as i32,
                "guest socket belongs to another process, not the owned backend"
            );
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("owned backend readiness timed out")
}
fn send_backend_ready(control: &mut UnixStream, execution: &str, child: u32) -> Result<()> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child, 0) };
    ensure!(fd >= 0, "actual backend pidfd is unavailable");
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    super::handoff::send_control_packet(control, &execution, Some(&fd))
}
fn drop_privileges() -> std::io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x20080522,
        pid: 0,
    };
    let data = [
        Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn drop_profile_privileges(all_five: bool) -> std::io::Result<()> {
    if !all_five {
        return drop_privileges();
    }
    let proof = unsafe { super::network::linux::drop_all_capabilities_for_exec() }
        .map_err(std::io::Error::other)?;
    proof
        .verify_current_for_exec()
        .map_err(std::io::Error::other)
}
struct ProcessGuard(i32);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.0 > 0 {
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                let mut status = 0;
                while libc::waitpid(self.0, &mut status, 0) < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                }
            }
        }
    }
}

#[cfg(test)]
mod authority_monitor_tests {
    use super::*;
    struct TestAuthority(Arc<AtomicBool>);
    impl AttemptAuthority for TestAuthority {
        fn validate(&self, _: &ObservationBinding) -> Result<()> {
            ensure!(self.0.load(Ordering::Acquire), "test authority revoked");
            Ok(())
        }
    }
    fn binding() -> ObservationBinding {
        ObservationBinding {
            format_version: 1,
            attempt: crate::vfs::rpc::AttemptContext {
                execution_id: Uuid::new_v4().to_string(),
                operation_id: Uuid::new_v4().to_string(),
            },
            view_id: Uuid::new_v4().to_string(),
            owner: OwnerEpoch {
                owner: "monitor-test".into(),
                epoch: 1,
            },
            namespace: Uuid::new_v4().to_string(),
            profile: crate::protocol::ObjectRef::from_bytes(
                crate::protocol::ObjectKind::Blob,
                b"test",
            ),
            exclusion_policy_digest: "a".repeat(64),
        }
    }
    #[test]
    fn observed_authority_loss_is_sticky_and_never_revives_the_same_attempt() {
        // This pure monitor fixture does not enter a namespace or touch mounts.
        // Production constructs it only from the post-entry factory path.
        let live = Arc::new(AtomicBool::new(true));
        let monitor =
            AuthorityMonitor::start(Arc::new(TestAuthority(live.clone())), binding()).unwrap();
        live.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !monitor.authority.cancelled() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        live.store(true, Ordering::Release);
        assert!(monitor
            .authority
            .validate(&monitor.authority.binding)
            .is_err());
        assert!(monitor.check().is_err());
        monitor.stop.store(true, Ordering::Release);
        monitor.done.recv_timeout(AUTHORIZATION_SETTLE).unwrap();
    }
    #[test]
    fn denied_initial_authority_cannot_start_a_monitor_or_create_an_authorized_attempt() {
        assert!(AuthorityMonitor::start(
            Arc::new(TestAuthority(Arc::new(AtomicBool::new(false)))),
            binding()
        )
        .is_err());
    }
}
