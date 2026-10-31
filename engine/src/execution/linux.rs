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

// Every phase borrows the same authenticated deadline. None preserves legacy
// Binding2/3 behavior. Blocking filesystem/application calls cannot be killed by
// this guard: they are fenced before and after, and late effects remain recovery
// candidates rather than successful finalization.
#[derive(Clone, Default)]
struct FinalizationPhases(Option<super::finalization_budget::FinalizationBudget>);
impl FinalizationPhases {
    fn check(&self) -> Result<()> {
        if let Some(budget) = &self.0 {
            budget.remaining()?;
        }
        Ok(())
    }
    fn limit(&self, upper: Duration) -> Result<Duration> {
        match &self.0 {
            Some(budget) => budget.phase_limit(upper),
            None => Ok(upper),
        }
    }
    fn during<T>(&self, effect: impl FnOnce() -> Result<T>) -> Result<T> {
        self.check()?;
        let value = effect()?;
        self.check()?;
        Ok(value)
    }
}

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
    finalization: FinalizationPhases,
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
    fn close_with_budget(&self, phases: &FinalizationPhases) -> Result<()> {
        phases.during(|| match self {
            Self::Complete(rpc) => rpc.await_closed(phases.limit(Duration::from_secs(35))?),
            Self::Partial { rpc, settled } => {
                let mut receipt = settled
                    .lock()
                    .map_err(|_| anyhow::anyhow!("partial settlement ownership unavailable"))?;
                if receipt.is_none() {
                    *receipt = Some(rpc.await_closed(phases.limit(Duration::from_secs(35))?)?);
                }
                ensure!(
                    receipt.as_ref().unwrap().binding_digest() == rpc.binding().digest()?,
                    "partial settlement changed binding"
                );
                Ok(())
            }
        })
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

// All partial source checks share one actual callback worker. A request or
// caller timeout never releases its charge until that worker settles it.
const PARTIAL_SOURCE_ADMISSIONS: usize = 4;
const PARTIAL_SOURCE_IDLE_POLL: Duration = Duration::from_millis(500);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PartialSourceThreadStage {
    Observer,
    Worker,
}
type PartialSourceWork = Box<dyn FnOnce() + Send + 'static>;
struct PartialSourceState {
    source: Arc<dyn PartialAttemptAuthority>,
    binding: PartialBinding,
    revoked: AtomicBool,
    closing: AtomicBool,
    admitted: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicUsize,
    budget: std::sync::Mutex<Option<super::finalization_budget::FinalizationBudget>>,
}
impl PartialSourceState {
    fn fence(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    fn observe_budget(
        &self,
        budget: Option<&super::finalization_budget::FinalizationBudget>,
    ) -> Result<()> {
        if let Some(budget) = budget {
            let mut owned = self
                .budget
                .lock()
                .map_err(|_| anyhow::anyhow!("partial source budget unavailable"))?;
            if let Some(previous) = owned.as_ref() {
                ensure!(
                    previous.constraint() == budget.constraint(),
                    "partial source budget cannot restart"
                );
            } else {
                *owned = Some(budget.clone());
            }
        }
        Ok(())
    }
    fn check(&self) -> Result<()> {
        let budget = self
            .budget
            .lock()
            .map_err(|_| anyhow::anyhow!("partial source budget unavailable"))?
            .clone();
        if let Some(budget) = budget {
            if budget.remaining().is_err() {
                self.fence();
            }
        }
        // Caller-side checks invoke no source trait hooks. Actual Native
        // cancellation is a trusted local clock/Atomic check, but even a bad
        // hook must run only inside the charged callback worker.
        ensure!(
            !self.revoked.load(Ordering::Acquire) && !self.closing.load(Ordering::Acquire),
            "partial source authorization retired"
        );
        Ok(())
    }
    fn actual(&self) -> Result<()> {
        self.check()?;
        self.active.fetch_add(1, Ordering::AcqRel);
        struct Active<'a>(&'a PartialSourceState);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.active.fetch_sub(1, Ordering::AcqRel);
                if std::thread::panicking() {
                    self.0.fence();
                }
            }
        }
        let _active = Active(self);
        if self.source.cancelled() {
            self.fence();
        }
        self.check()?;
        if self.source.validate(&self.binding).is_err() {
            self.fence();
        }
        if self.source.cancelled() {
            self.fence();
        }
        self.check()
    }
}
struct PartialSourceAdmission(Arc<PartialSourceState>);
impl Drop for PartialSourceAdmission {
    fn drop(&mut self) {
        self.0.admitted.fetch_sub(1, Ordering::AcqRel);
    }
}
struct PartialSourceCheck {
    reply: std::sync::mpsc::SyncSender<bool>,
    deadline: Instant,
    budget: Option<super::finalization_budget::FinalizationBudget>,
    _admission: PartialSourceAdmission,
}
impl PartialSourceCheck {
    fn remaining(&self) -> Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "partial source check timed out");
        Ok(match &self.budget {
            Some(b) => remaining.min(b.remaining()?),
            None => remaining,
        })
    }
}
struct LivePartialAuthority {
    state: Arc<PartialSourceState>,
    requests: std::sync::mpsc::SyncSender<PartialSourceCheck>,
}
impl LivePartialAuthority {
    fn current(
        &self,
        budget: Option<&super::finalization_budget::FinalizationBudget>,
    ) -> Result<()> {
        let result = (|| {
            self.state.observe_budget(budget)?;
            let budget = self
                .state
                .budget
                .lock()
                .map_err(|_| anyhow::anyhow!("partial source budget unavailable"))?
                .clone();
            self.state.check()?;
            let phase = match budget.as_ref() {
                Some(b) => b.phase_limit(AUTHORIZATION_SETTLE)?,
                None => AUTHORIZATION_SETTLE,
            };
            self.state
                .admitted
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    (n < PARTIAL_SOURCE_ADMISSIONS).then_some(n + 1)
                })
                .map_err(|_| anyhow::anyhow!("partial source validation busy"))?;
            let admission = PartialSourceAdmission(self.state.clone());
            let deadline = Instant::now()
                .checked_add(phase)
                .context("partial source deadline overflow")?;
            let (reply, receive) = std::sync::mpsc::sync_channel(1);
            self.requests
                .try_send(PartialSourceCheck {
                    reply,
                    deadline,
                    budget: budget.clone(),
                    _admission: admission,
                })
                .map_err(|_| anyhow::anyhow!("partial source queue unavailable"))?;
            loop {
                self.state.check()?;
                let mut remaining = deadline.saturating_duration_since(Instant::now());
                if let Some(b) = budget.as_ref() {
                    remaining = remaining.min(b.remaining()?);
                }
                ensure!(
                    !remaining.is_zero(),
                    "partial source validation did not settle"
                );
                match receive.recv_timeout(remaining.min(Duration::from_millis(10))) {
                    Ok(true) => {
                        self.state.check()?;
                        if let Some(b) = budget.as_ref() {
                            b.remaining()?;
                        }
                        ensure!(Instant::now() < deadline, "late partial source reply");
                        return Ok(());
                    }
                    Ok(false) => anyhow::bail!("partial source authorization denied"),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => anyhow::bail!("partial source worker unavailable"),
                }
            }
        })();
        if result.is_err() {
            self.state.fence();
        }
        result
    }
}
impl PartialAttemptAuthority for LivePartialAuthority {
    fn validate(&self, binding: &PartialBinding) -> Result<()> {
        if binding != &self.state.binding {
            self.state.fence();
        }
        self.current(None)
    }
    fn cancelled(&self) -> bool {
        self.state.check().is_err()
    }
}
struct PartialSourceJoin {
    // The fixed observer performs the REAL worker join, including TLS. No
    // deadline waiter joins this observer; its custody remains retained.
    _observer: std::thread::JoinHandle<()>,
    done: std::sync::mpsc::Receiver<bool>,
    joined: bool,
    failed: bool,
}
struct PartialAuthorityMonitor {
    authority: Arc<LivePartialAuthority>,
    join: std::sync::Mutex<PartialSourceJoin>,
}
impl PartialAuthorityMonitor {
    fn start(source: Arc<dyn PartialAttemptAuthority>, binding: PartialBinding) -> Result<Self> {
        Self::start_with_spawn(source, binding, |stage, work| {
            std::thread::Builder::new()
                .name(
                    match stage {
                        PartialSourceThreadStage::Observer => "rudder-partial-source-join",
                        PartialSourceThreadStage::Worker => "rudder-partial-source",
                    }
                    .into(),
                )
                .spawn(work)
        })
    }
    fn start_with_spawn(
        source: Arc<dyn PartialAttemptAuthority>,
        binding: PartialBinding,
        mut spawn: impl FnMut(
            PartialSourceThreadStage,
            PartialSourceWork,
        ) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Self> {
        binding.validate()?;
        let state = Arc::new(PartialSourceState {
            source,
            binding,
            revoked: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            admitted: std::sync::atomic::AtomicUsize::new(0),
            active: std::sync::atomic::AtomicUsize::new(0),
            budget: std::sync::Mutex::new(None),
        });
        let (requests, receive) =
            std::sync::mpsc::sync_channel::<PartialSourceCheck>(PARTIAL_SOURCE_ADMISSIONS);
        let (transfer, owned_worker) =
            std::sync::mpsc::sync_channel::<std::thread::JoinHandle<()>>(1);
        let (send, done) = std::sync::mpsc::sync_channel(1);
        let retained = state.clone();
        // Spawn the fixed owner FIRST. Before receiving the actual JoinHandle
        // it executes no source/cancellation callback or user cleanup.
        let observer = spawn(
            PartialSourceThreadStage::Observer,
            Box::new(move || {
                let success = match owned_worker.recv() {
                    Ok(worker) => worker.join().is_ok(),
                    Err(_) => false,
                };
                if !success {
                    retained.fence();
                }
                drop(retained);
                let _ = send.send(success);
            }),
        )?;
        let (release_start, may_start) = std::sync::mpsc::sync_channel::<()>(1);
        let running = state.clone();
        let worker = spawn(
            PartialSourceThreadStage::Worker,
            Box::new(move || {
                // No callback (including cancelled) can run before recoverable
                // ownership transfer. Closing this gate on failure is inert.
                if may_start.recv().is_err() {
                    return;
                }
                struct Exit(Arc<PartialSourceState>);
                impl Drop for Exit {
                    fn drop(&mut self) {
                        if std::thread::panicking() {
                            self.0.fence();
                        }
                    }
                }
                let _exit = Exit(running.clone());
                while running.check().is_ok() {
                    match receive.recv_timeout(PARTIAL_SOURCE_IDLE_POLL) {
                        Ok(request) => {
                            let granted = request.remaining().is_ok()
                                && running.actual().is_ok()
                                && request.remaining().is_ok()
                                && running.check().is_ok();
                            if !granted {
                                running.fence();
                            }
                            let _ = request.reply.try_send(granted);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if running.actual().is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                while let Ok(request) = receive.try_recv() {
                    let _ = request.reply.try_send(false);
                }
            }),
        );
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                state.closing.store(true, Ordering::Release);
                drop(release_start);
                drop(transfer); // Observer has no worker/callback to wait on.
                                // Only known startup channel code ran; no callback/TLS user
                                // work was admitted. Reap the actual observer on this failure.
                let _ = observer.join();
                return Err(error.into());
            }
        };
        if let Err(error) = transfer.try_send(worker) {
            state.closing.store(true, Ordering::Release);
            drop(release_start);
            // Recover and reap the ACTUAL handle. Its startup gate was never
            // released, so neither callbacks nor their TLS could have run.
            let worker = match error {
                std::sync::mpsc::TrySendError::Full(worker)
                | std::sync::mpsc::TrySendError::Disconnected(worker) => worker,
            };
            let _ = worker.join();
            drop(transfer);
            let _ = observer.join();
            anyhow::bail!("partial source worker ownership transfer failed");
        }
        drop(transfer);
        let monitor = Self {
            authority: Arc::new(LivePartialAuthority { state, requests }),
            join: std::sync::Mutex::new(PartialSourceJoin {
                _observer: observer,
                done,
                joined: false,
                failed: false,
            }),
        };
        release_start
            .try_send(())
            .map_err(|_| anyhow::anyhow!("partial source startup gate failed"))?;
        monitor.validate_current()?;
        Ok(monitor)
    }
    fn observe_budget(
        &self,
        budget: Option<&super::finalization_budget::FinalizationBudget>,
    ) -> Result<()> {
        let result = self.authority.state.observe_budget(budget);
        if result.is_err() {
            self.authority.state.fence();
        }
        result
    }
    fn check(&self) -> Result<()> {
        self.authority.state.check()
    }
    fn validate_current(&self) -> Result<()> {
        self.authority.current(None)
    }
    fn current(
        &self,
        budget: Option<&super::finalization_budget::FinalizationBudget>,
    ) -> Result<()> {
        self.authority.current(budget)
    }
    fn close(&self) {
        self.authority.state.closing.store(true, Ordering::Release);
    }
    fn settle(
        &self,
        limit: Duration,
        budget: Option<&super::finalization_budget::FinalizationBudget>,
    ) -> Result<()> {
        self.close();
        let limit = match budget {
            Some(b) => b.phase_limit(limit)?,
            None => limit,
        };
        let deadline = Instant::now()
            .checked_add(limit)
            .context("partial source settle deadline overflow")?;
        let mut join = self
            .join
            .try_lock()
            .map_err(|_| anyhow::anyhow!("partial source join owner unavailable"))?;
        ensure!(!join.failed, "partial source worker failed");
        while !join.joined {
            let mut remaining = deadline.saturating_duration_since(Instant::now());
            if let Some(b) = budget {
                remaining = remaining.min(b.remaining()?);
            }
            ensure!(!remaining.is_zero(), "partial source work still owned");
            match join
                .done
                .recv_timeout(remaining.min(Duration::from_millis(10)))
            {
                Ok(true) => join.joined = true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                _ => {
                    join.failed = true;
                    anyhow::bail!("partial source worker failed");
                }
            }
        }
        ensure!(
            self.authority.state.active.load(Ordering::Acquire) == 0
                && self.authority.state.admitted.load(Ordering::Acquire) == 0,
            "partial source work has not settled"
        );
        Ok(())
    }
}
impl Drop for PartialAuthorityMonitor {
    fn drop(&mut self) {
        self.close();
    }
}
struct SourceFinalizationOwner {
    monitor: PartialAuthorityMonitor,
    budget: Option<super::finalization_budget::FinalizationBudget>,
}
impl SourceFinalizationOwner {
    fn current(&self) -> Result<()> {
        self.monitor.current(self.budget.as_ref())
    }
    fn during<T>(&self, effect: impl FnOnce() -> Result<T>) -> Result<T> {
        self.current()?;
        let value = effect()?;
        self.current()?;
        Ok(value)
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
            Self::Partial(_) => {
                anyhow::bail!("partial source owner must survive publication and receipt")
            }
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
            rpc.close_with_budget(&self.finalization)?;
        }
        Ok(())
    }
    fn close_rpc_with_budget(&self, phases: &FinalizationPhases) -> Result<()> {
        phases.during(|| {
            if let Some(rpc) = &self.rpc {
                rpc.close_with_budget(phases)?;
            }
            Ok(())
        })
    }
    fn unmount(&mut self, phases: &FinalizationPhases) -> Result<VfsView> {
        phases.during(|| self.close_application())?;
        self.close_rpc_with_budget(phases)?;
        phases.during(|| {
            self.mounted
                .as_mut()
                .context("execution mount absent")?
                .unmount()
        })
    }
}
impl Drop for ExecutionMount {
    fn drop(&mut self) {
        let finalization = self.finalization.clone();
        if finalization.during(|| self.close_application()).is_err() || self.close_rpc().is_err() {
            // An expired budget also takes this retained-mount failure path;
            // cleanup cannot start a fresh application settlement interval.
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
    finalization: FinalizationPhases,
    source: Option<SourceFinalizationOwner>,
    _source_workspace: Option<Arc<OwnedHelperWorkspace>>,
}
impl OwnedExecutionDrain {
    fn source_current(&self) -> Result<()> {
        self.finalization.check()?;
        if let Some(source) = &self.source {
            ensure!(
                self._source_workspace.is_some(),
                "partial source resources are not owned"
            );
            source.current()?;
        }
        self.finalization.check()
    }
    fn source_effect<T>(&self, effect: impl FnOnce() -> Result<T>) -> Result<T> {
        self.finalization.during(|| match &self.source {
            Some(source) => source.during(effect),
            None => effect(),
        })
    }
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
        self.source_current()?;
        ensure!(
            self.source.is_some(),
            "remote drain lacks live source ownership"
        );
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
            ensure!(
                self.source.is_some() && self._source_workspace.is_some(),
                "partial commit lacks actual source/resource ownership"
            );
            self.source_current()?;
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
        self.source_current()?;
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
        self.source_current()?;
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
            finalization: FinalizationPhases::default(),
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
    fn expired_drop_budget_never_starts_application_settlement() {
        use super::super::finalization_budget::FinalizationBudget;
        let attempts = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let budget = FinalizationBudget::begin(Duration::from_millis(1)).unwrap();
        let mounted = ExecutionMount {
            finalization: FinalizationPhases(Some(budget.clone())),
            mounted: None,
            rpc: None,
            application: Some(ApplicationLifecycle::new(Box::new(Service {
                file: None,
                attempts: attempts.clone(),
                closed: closed.clone(),
                dropped: Arc::new(AtomicBool::new(false)),
                fail: true,
            }))),
        };
        let guard = Instant::now() + Duration::from_secs(2);
        while budget.remaining().is_ok() {
            assert!(
                Instant::now() < guard,
                "host finalization clock failed to expire"
            );
            std::thread::yield_now();
        }
        drop(mounted);
        assert_eq!(attempts.load(Ordering::Acquire), 0);
        assert!(!closed.load(Ordering::Acquire));
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
            finalization: FinalizationPhases::default(),
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
    // Codec support grants no execution authority. Keep Spec5 out of legacy
    // dispatch before namespace entry, factories or any execution effects.
    ensure!(
        spec.format_version != 5,
        "execution specification5 awaits owned source and shutdown lifetimes"
    );
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
        let mut monitor = match (&spec.workspace_observation, &services) {
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
                    ensure!(spec.format_version == 4, "shared partial authority requires specification4");
                    WorkspaceMonitor::Partial(PartialAuthorityMonitor::start(
                        authority.clone(),
                        partial_binding
                            .clone()
                            .context("partial authority requires a sealed remote selection")?,
                    )?)
                }
                HelperWorkspaceServices::PartialSeparated { source, .. } => {
                    ensure!(spec.format_version == 5, "separated source authority requires specification5");
                    WorkspaceMonitor::Partial(PartialAuthorityMonitor::start(
                        source.clone(),
                        partial_binding.clone().context("host source requires a sealed remote selection")?,
                    )?)
                }
            }),
            (None, None) => None,
            _ => anyhow::bail!("workspace helper service/profile mismatch"),
        };
        unsafe {
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        }
        let workspace = Arc::new(
            state.open_workspace(
                &spec,
                services
                    .as_mut()
                    .and_then(|services| services.workspace_factory.take()),
            )?,
        );
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
                finalization: FinalizationPhases::default(),
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
                    (
                        HelperWorkspaceServices::PartialSeparated { ingress, .. },
                        WorkspaceMonitor::Partial(_),
                    ) => {
                        ensure!(spec.format_version == 5, "separated ingress requires specification5");
                        let rpc = PartialRpc::bind(
                            view.clone(),
                            PartialRpcConfig { authority: ingress.clone(), limits: PartialRpcLimits::default() },
                            &workspace.endpoint_directory,
                        )?;
                        ensure!(Some(rpc.binding()) == partial_binding.as_ref(), "partial ingress differs from sealed selection");
                        super::root::validate_partial_workspace_projection(workspace, rpc.binding())?;
                        mounted.rpc = Some(WorkspaceRpc::Partial { rpc, settled: std::sync::Mutex::new(None) });
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
                    let stopping = control.poll_stop()?;
                    if control.binding.format_version == 4 {
                        // Retain the same deadline on failure cleanup too; Drop
                        // must not restart a fresh 35-second RPC close wait.
                        mounted.finalization = FinalizationPhases(control.finalization_budget());
                    }
                    if let Some(WorkspaceMonitor::Partial(source)) = &monitor {
                        source.observe_budget(control.finalization_budget().as_ref())?;
                    }
                    if stopping && !stop_sent {
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
            let finalization = FinalizationPhases(match control.as_ref() {
                Some(control) if control.binding.format_version == 4 => Some(
                    control
                        .finalization_budget()
                        .context("authenticated finalization budget absent")?,
                ),
                _ => None,
            });
            finalization.during(|| mounted.close_application())?;
            mounted.close_rpc_with_budget(&finalization)?;
            if let Some(monitor) = &monitor {
                monitor.check()?;
            }
            let _view = mounted.unmount(&finalization)?;
            let source = match monitor.take() {
                Some(WorkspaceMonitor::Partial(source)) => Some(SourceFinalizationOwner {
                    monitor: source,
                    budget: control
                        .as_ref()
                        .and_then(|control| control.finalization_budget()),
                }),
                other => {
                    monitor = other;
                    None
                }
            };
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
                finalization,
                source,
                _source_workspace: partial_binding.as_ref().map(|_| workspace.clone()),
            };
            if let Some(monitor) = &monitor {
                monitor.validate_current()?;
            }
            let committed = if partial_binding.is_some() {
                let (committed, frozen) = owned_drain.source_effect(|| {
                    owned_drain.validate_remote_finalization(state.record(), &workspace)?;
                    let request = crate::vfs::RemoteFreezeRequest {
                        format_version: 1,
                        intent_id: Uuid::new_v4().to_string(),
                        capture: crate::vfs::PartialCaptureRequest {
                            nonce: owned_drain.authenticated_stop()?.request_id.clone(),
                            expected_begin: view.dirty_cursor()?,
                            expected_head: view
                                .partial_capture_head()?
                                .map(|head| head.request_nonce),
                        },
                    };
                    let mut frozen = owned_drain.finalization.during(|| {
                        view.freeze_remote(
                            &request,
                            owned_drain.finalization.limit(Duration::from_secs(35))?,
                        )
                    })?;
                    let committed = state.finalize_remote(
                        &workspace,
                        &owned_drain,
                        &mut frozen,
                        crate::vfs::PartialCaptureBudget::default(),
                    )?;
                    frozen.check_for(view)?;
                    Ok((committed, frozen))
                })?;
                owned_drain.frozen = Some(frozen);
                committed
            } else {
                owned_drain.source_effect(|| state.finalize_local(&workspace, drain.agent_exit))?
            };
            if let Some(monitor) = &monitor {
                monitor.validate_current()?;
            }
            if let Some(monitor) = &monitor {
                monitor.finish()?;
            }
            owned_drain.source_effect(|| state.verify_commit_current(&committed))?;
            owned_drain.validate_publication(&committed)?;
            if let Some(control) = &mut control {
                // A sent packet remains provisional for the parent. Loss during
                // send makes this helper return Err; it cannot recall bytes.
                owned_drain.source_effect(|| control.shutdown_receipt(&committed, &owned_drain))?;
            }
            owned_drain.source_current()?;
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
                if let ExecutionOutcome::Finalized(finalized) = &value {
                    finalized._drain.finalization.check()?;
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

#[cfg(test)]
mod partial_source_lifetime_tests {
    use super::*;
    use fs2::FileExt;
    use std::{
        fs::OpenOptions,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        sync::{atomic::AtomicUsize, mpsc, Mutex},
    };

    struct Block {
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    struct Release {
        started: mpsc::Receiver<()>,
        release: Option<mpsc::SyncSender<()>>,
    }
    impl Release {
        fn wait(&self) {
            self.started.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        fn release(&mut self) {
            if let Some(send) = self.release.take() {
                let _ = send.send(());
            }
        }
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.release();
        }
    }
    struct HeldAuthority {
        owner: File,
        path: std::path::PathBuf,
        binding: PartialBinding,
        block: Mutex<Option<Block>>,
        calls: AtomicUsize,
        calls_at: Mutex<Vec<Instant>>,
        cancel_block: Mutex<Option<Block>>,
    }
    impl HeldAuthority {
        fn block_next(&self) -> Release {
            let (started, observed) = mpsc::sync_channel(1);
            let (release, wait) = mpsc::sync_channel(1);
            *self.block.lock().unwrap() = Some(Block {
                started,
                release: wait,
            });
            Release {
                started: observed,
                release: Some(release),
            }
        }
        fn block_cancel_next(&self) -> Release {
            let (started, observed) = mpsc::sync_channel(1);
            let (release, wait) = mpsc::sync_channel(1);
            *self.cancel_block.lock().unwrap() = Some(Block {
                started,
                release: wait,
            });
            Release {
                started: observed,
                release: Some(release),
            }
        }
        fn revoke(&self) {
            fs::rename(&self.path, self.path.with_extension("retired")).unwrap();
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&self.path)
                .unwrap();
        }
    }
    impl PartialAttemptAuthority for HeldAuthority {
        fn validate(&self, binding: &PartialBinding) -> Result<()> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            {
                let mut times = self.calls_at.lock().unwrap();
                assert!(times.len() < 256, "synthetic callback trace bounded");
                times.push(Instant::now());
            }
            let block = self.block.lock().unwrap().take();
            if let Some(block) = block {
                let _ = block.started.send(());
                let _ = block.release.recv();
            }
            let held = self.owner.metadata()?;
            let named = fs::symlink_metadata(&self.path)?;
            ensure!(
                binding == &self.binding
                    && held.is_file()
                    && named.is_file()
                    && held.dev() == named.dev()
                    && held.ino() == named.ino()
                    && named.nlink() == 1,
                "actual source owner FD was replaced"
            );
            Ok(())
        }
        fn cancelled(&self) -> bool {
            let block = self.cancel_block.lock().unwrap().take();
            if let Some(block) = block {
                let _ = block.started.send(());
                let _ = block.release.recv();
            }
            false
        }
    }
    fn binding() -> PartialBinding {
        let view_id = Uuid::new_v4().to_string();
        let execution_id = Uuid::new_v4().to_string();
        let operation_id = Uuid::new_v4().to_string();
        let owner = OwnerEpoch {
            owner: "source-lifetime".into(),
            epoch: 1,
        };
        PartialBinding {
            protocol: crate::vfs::partial_rpc::PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: "b".repeat(64),
            root: crate::remote_root::RootBinding {
                scope: crate::remote_cache::CacheScope {
                    provider_id: "fixture".into(),
                    tenant_id: Uuid::new_v4().to_string(),
                },
                root_id: "source".into(),
                root: crate::protocol::ObjectRef::from_bytes(
                    crate::protocol::ObjectKind::RunnableRoot,
                    b"fixture",
                ),
            },
            attempt: crate::remote_root::AttemptBinding {
                project_id: "project".into(),
                run_id: "run".into(),
                run_epoch: 1,
                owner: owner.clone(),
                execution_id: execution_id.clone(),
                operation_id: operation_id.clone(),
                view_id: view_id.clone(),
            },
            observation: ObservationBinding {
                format_version: 1,
                attempt: crate::vfs::rpc::AttemptContext {
                    execution_id,
                    operation_id,
                },
                view_id,
                owner,
                namespace: Uuid::new_v4().to_string(),
                profile: crate::protocol::ObjectRef::from_bytes(
                    crate::protocol::ObjectKind::Blob,
                    b"profile",
                ),
                exclusion_policy_digest: crate::vfs::exclusion_policy_digest(
                    &crate::store::ImportPolicy::default(),
                )
                .unwrap(),
            },
        }
    }
    struct Fixture {
        root: tempfile::TempDir,
        authority: Arc<HeldAuthority>,
        contender: File,
    }
    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("owner.lock");
            let owner = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            owner.lock_exclusive().unwrap();
            let contender = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            Self {
                root,
                authority: Arc::new(HeldAuthority {
                    owner,
                    path,
                    binding: binding(),
                    block: Mutex::new(None),
                    calls: AtomicUsize::new(0),
                    calls_at: Mutex::new(Vec::new()),
                    cancel_block: Mutex::new(None),
                }),
                contender,
            }
        }
        fn owner(
            &self,
            budget: Option<super::super::finalization_budget::FinalizationBudget>,
        ) -> SourceFinalizationOwner {
            let monitor = PartialAuthorityMonitor::start(
                self.authority.clone(),
                self.authority.binding.clone(),
            )
            .unwrap();
            monitor.observe_budget(budget.as_ref()).unwrap();
            SourceFinalizationOwner { monitor, budget }
        }
    }
    fn wait_for(check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !check() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn loss_before_final_check_prevents_effect_without_a_saved_grace_permission() {
        let fixture = Fixture::new();
        let owner = fixture.owner(None);
        owner.current().unwrap();
        fixture.authority.revoke();
        let ack = fixture.root.path().join("ack");
        assert!(owner
            .during(|| {
                fs::write(&ack, b"forbidden")?;
                Ok(())
            })
            .is_err());
        assert!(!ack.exists());
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
    }
    #[test]
    fn source_loss_during_durable_final_check_retains_candidate_but_refuses_ack() {
        let fixture = Fixture::new();
        let owner = fixture.owner(None);
        let candidate = fixture.root.path().join("candidate");
        let ack = fixture.root.path().join("ack");
        let checked = owner.during(|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&candidate)?;
            file.write_all(b"retained candidate data")?;
            file.sync_all()?;
            fixture.authority.revoke();
            Ok(())
        });
        if checked.is_ok() {
            fs::write(&ack, b"not permitted").unwrap();
        }
        assert!(checked.is_err());
        assert_eq!(fs::read(candidate).unwrap(), b"retained candidate data");
        assert!(!ack.exists());
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
    }
    #[test]
    fn packet_already_sent_is_provisional_and_loss_during_send_refuses_final_success() {
        let fixture = Fixture::new();
        let owner = fixture.owner(None);
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let result = owner.during(|| {
            writer.write_all(b"provisional-record2")?;
            fixture.authority.revoke();
            Ok(())
        });
        let mut packet = [0; 19];
        reader.read_exact(&mut packet).unwrap();
        assert_eq!(&packet, b"provisional-record2");
        assert!(
            result.is_err(),
            "packet bytes must never imply final source authority"
        );
        // The future Binding4 parent must still join actual successful helper
        // exit +fresh issuer custody. This fixture claims no packet recall.
        assert!(fixture
            .authority
            .validate(&fixture.authority.binding)
            .is_err());
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
    }
    #[test]
    fn early_monitor_retirement_negative_control_leaks_an_unchecked_packet() {
        let old = Fixture::new();
        let monitor =
            PartialAuthorityMonitor::start(old.authority.clone(), old.authority.binding.clone())
                .unwrap();
        monitor.validate_current().unwrap();
        monitor.settle(Duration::from_secs(2), None).unwrap();
        old.authority.revoke();
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // Reproduce the old ordering: monitor is retired, later write has no
        // actual source gate. Current held-FD authority already rejects it.
        assert!(old.authority.validate(&old.authority.binding).is_err());
        writer.write_all(b"bad").unwrap();
        let mut bytes = [0; 3];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"bad");
        let fixed = Fixture::new();
        let owner = fixed.owner(None);
        fixed.authority.revoke();
        assert!(owner
            .during(|| {
                writer.write_all(b"ack")?;
                Ok(())
            })
            .is_err());
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
    }
    #[test]
    fn timed_out_actual_callback_keeps_charge_fd_and_join_owner_until_real_release() {
        use super::super::finalization_budget::FinalizationBudget;
        let fixture = Fixture::new();
        let mut owner = fixture.owner(None);
        let mut blocked = fixture.authority.block_next();
        // Let the existing single background callback enter first. Start the
        // authenticated time constraint only after its actual admission is held.
        blocked.wait();
        owner.budget = Some(FinalizationBudget::begin(Duration::from_millis(40)).unwrap());
        let calling = owner.monitor.authority.clone();
        let budget = owner.budget.clone();
        let request = std::thread::spawn(move || calling.current(budget.as_ref()));
        assert!(request.join().unwrap().is_err());
        let state = &owner.monitor.authority.state;
        assert_eq!(state.active.load(Ordering::Acquire), 1);
        assert_eq!(state.admitted.load(Ordering::Acquire), 1);
        assert!(fixture.contender.try_lock_exclusive().is_err());
        assert!(owner
            .monitor
            .settle(Duration::from_millis(20), None)
            .is_err());
        assert!(!owner.monitor.join.lock().unwrap().joined);
        blocked.release();
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
        assert_eq!(state.active.load(Ordering::Acquire), 0);
        assert_eq!(state.admitted.load(Ordering::Acquire), 0);
        assert!(owner.current().is_err());
    }
    #[test]
    fn bounded_queue_does_not_replace_or_forget_an_actual_stalled_callback() {
        let fixture = Fixture::new();
        let owner = fixture.owner(None);
        let mut blocked = fixture.authority.block_next();
        let mut callers = Vec::new();
        for _ in 0..PARTIAL_SOURCE_ADMISSIONS {
            let authority = owner.monitor.authority.clone();
            callers.push(std::thread::spawn(move || authority.current(None)));
        }
        blocked.wait();
        wait_for(|| {
            owner
                .monitor
                .authority
                .state
                .admitted
                .load(Ordering::Acquire)
                == PARTIAL_SOURCE_ADMISSIONS
        });
        assert!(owner.current().is_err());
        assert_eq!(
            owner.monitor.authority.state.active.load(Ordering::Acquire),
            1
        );
        assert_eq!(
            owner
                .monitor
                .authority
                .state
                .admitted
                .load(Ordering::Acquire),
            PARTIAL_SOURCE_ADMISSIONS
        );
        assert!(fixture.contender.try_lock_exclusive().is_err());
        blocked.release();
        for caller in callers {
            assert!(caller.join().unwrap().is_err());
        }
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
        assert_eq!(
            owner
                .monitor
                .authority
                .state
                .admitted
                .load(Ordering::Acquire),
            0
        );
    }
    #[test]
    fn original_budget_cannot_be_restarted_at_publication_even_with_a_live_owner_fd() {
        use super::super::finalization_budget::FinalizationBudget;
        let fixture = Fixture::new();
        let owner = fixture.owner(Some(
            FinalizationBudget::begin(Duration::from_millis(20)).unwrap(),
        ));
        let calls = fixture.authority.calls.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(25));
        assert!(owner.current().is_err());
        assert_eq!(fixture.authority.calls.load(Ordering::Acquire), calls);
        assert!(fixture.contender.try_lock_exclusive().is_err());
        assert!(owner.monitor.validate_current().is_err());
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
    }
    #[test]
    fn source_handle_stays_owned_through_final_effect_and_actual_monitor_settlement() {
        let fixture = Fixture::new();
        let root = fixture.root;
        let weak = Arc::downgrade(&fixture.authority);
        let monitor = PartialAuthorityMonitor::start(
            fixture.authority.clone(),
            fixture.authority.binding.clone(),
        )
        .unwrap();
        let owner = SourceFinalizationOwner {
            monitor,
            budget: None,
        };
        drop(fixture.authority);
        owner
            .during(|| {
                fs::write(root.path().join("receipt-phase"), b"candidate")?;
                Ok(())
            })
            .unwrap();
        assert!(fixture.contender.try_lock_exclusive().is_err());
        assert!(weak.upgrade().is_some());
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
        assert!(weak.upgrade().is_some());
        drop(owner);
        wait_for(|| weak.upgrade().is_none());
        fixture.contender.try_lock_exclusive().unwrap();
        FileExt::unlock(&fixture.contender).unwrap();
    }
    #[derive(Clone, Copy)]
    enum StartupFailure {
        Observer,
        Worker,
        Transfer,
    }
    fn failed_start(fixture: &Fixture, failure: StartupFailure) {
        let started = Arc::new(AtomicUsize::new(0));
        let exited = Arc::new(AtomicUsize::new(0));
        let observed = started.clone();
        let completed = exited.clone();
        let result = PartialAuthorityMonitor::start_with_spawn(
            fixture.authority.clone(),
            fixture.authority.binding.clone(),
            move |stage, work| {
                if matches!(
                    (failure, stage),
                    (StartupFailure::Observer, PartialSourceThreadStage::Observer)
                        | (StartupFailure::Worker, PartialSourceThreadStage::Worker)
                ) {
                    return Err(std::io::Error::other("synthetic startup spawn refusal"));
                }
                let work: PartialSourceWork = if matches!(
                    (failure, stage),
                    (StartupFailure::Transfer, PartialSourceThreadStage::Observer)
                ) {
                    // Drop the REAL receiver before transfer, then run a real
                    // inert observer thread. The recovered worker remains gated.
                    drop(work);
                    Box::new(|| {})
                } else {
                    work
                };
                let started = observed.clone();
                let exited = completed.clone();
                std::thread::Builder::new()
                    .name("synthetic-source-startup".into())
                    .spawn(move || {
                        struct Exit(Arc<AtomicUsize>);
                        impl Drop for Exit {
                            fn drop(&mut self) {
                                self.0.fetch_add(1, Ordering::AcqRel);
                            }
                        }
                        let _exit = Exit(exited);
                        started.fetch_add(1, Ordering::AcqRel);
                        work();
                    })
            },
        );
        assert!(result.is_err());
        let expected = match failure {
            StartupFailure::Observer => 0,
            StartupFailure::Worker => 1,
            StartupFailure::Transfer => 2,
        };
        assert_eq!(started.load(Ordering::Acquire), expected);
        assert_eq!(exited.load(Ordering::Acquire), expected);
        assert_eq!(fixture.authority.calls.load(Ordering::Acquire), 0);
        assert_eq!(
            Arc::strong_count(&fixture.authority),
            1,
            "no detached startup thread may retain a lost source owner"
        );
        assert!(fixture.contender.try_lock_exclusive().is_err());
    }
    #[test]
    fn observer_spawn_failure_creates_no_worker_and_invokes_no_source_callback() {
        let fixture = Fixture::new();
        failed_start(&fixture, StartupFailure::Observer);
    }
    #[test]
    fn worker_spawn_failure_reaps_inert_observer_without_a_source_callback() {
        let fixture = Fixture::new();
        failed_start(&fixture, StartupFailure::Worker);
    }
    #[test]
    fn failed_handle_transfer_recovers_and_reaps_the_actual_gated_worker() {
        let fixture = Fixture::new();
        failed_start(&fixture, StartupFailure::Transfer);
    }
    #[test]
    fn normal_finalization_twenty_seven_fresh_checks_are_an_unslept_measured_burst() {
        use super::super::finalization_budget::FinalizationBudget;
        let fixture = Fixture::new();
        let owner = fixture.owner(None);
        let budget = FinalizationBudget::begin(Duration::from_secs(10)).unwrap();
        owner.monitor.observe_budget(Some(&budget)).unwrap();
        let before = fixture.authority.calls.load(Ordering::Acquire);
        let start = Instant::now();
        // Exact normal path: pre-freeze1 +State12 +outer verify2 +publication3
        // +receipt5 +last current1 +outcome3. This load fixture executes fresh
        // callbacks, not a fake drain, namespace or simulated successful ACK.
        for _ in 0..27 {
            owner.monitor.current(Some(&budget)).unwrap();
        }
        let elapsed = start.elapsed();
        let callbacks = fixture.authority.calls.load(Ordering::Acquire) - before;
        assert!(
            callbacks >= 27,
            "no freshness check may use a cached positive"
        );
        let times = fixture.authority.calls_at.lock().unwrap();
        let maximum = times
            .iter()
            .enumerate()
            .map(|(index, began)| {
                times[index..]
                    .iter()
                    .take_while(|at| at.duration_since(*began) < Duration::from_secs(1))
                    .count()
            })
            .max()
            .unwrap();
        eprintln!("synthetic finalization load: foreground=27 actual_callbacks={callbacks} elapsed_us={} max_one_second={maximum}", elapsed.as_micros());
        // Root's distinct host endpoint is to preserve16/s average with64 burst;
        // actual Native token-bucket/transport integration is a separate check.
        assert!(maximum <= 64);
        drop(times);
        assert!(fixture.contender.try_lock_exclusive().is_err());
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
    }
    #[test]
    fn blocked_cancellation_hook_runs_only_on_the_actual_charged_worker() {
        use super::super::finalization_budget::FinalizationBudget;
        let fixture = Fixture::new();
        let owner = fixture.owner(None);
        let mut blocked = fixture.authority.block_cancel_next();
        // A real background callback now blocks INSIDE cancelled(), not on a
        // caller-side check. It still owns the fixed callback worker and FD.
        blocked.wait();
        let budget = FinalizationBudget::begin(Duration::from_millis(40)).unwrap();
        assert!(owner.monitor.current(Some(&budget)).is_err());
        assert_eq!(
            owner.monitor.authority.state.active.load(Ordering::Acquire),
            1
        );
        assert_eq!(
            owner
                .monitor
                .authority
                .state
                .admitted
                .load(Ordering::Acquire),
            1
        );
        assert!(fixture.contender.try_lock_exclusive().is_err());
        assert!(owner
            .monitor
            .settle(Duration::from_millis(20), None)
            .is_err());
        blocked.release();
        owner.monitor.settle(Duration::from_secs(2), None).unwrap();
        assert_eq!(
            owner.monitor.authority.state.active.load(Ordering::Acquire),
            0
        );
        assert_eq!(
            owner
                .monitor
                .authority
                .state
                .admitted
                .load(Ordering::Acquire),
            0
        );
    }
}

#[cfg(test)]
mod finalization_phase_tests {
    use super::super::finalization_budget::FinalizationBudget;
    use super::*;

    #[test]
    fn legacy_phases_keep_existing_timeout_and_effect_result() {
        let phases = FinalizationPhases::default();
        assert_eq!(
            phases.limit(Duration::from_secs(35)).unwrap(),
            Duration::from_secs(35)
        );
        assert_eq!(phases.during(|| Ok(17)).unwrap(), 17);
    }

    #[test]
    fn shared_cancellation_blocks_later_phase_before_any_effect() {
        let budget = FinalizationBudget::begin(Duration::from_secs(1)).unwrap();
        let phases = FinalizationPhases(Some(budget.clone()));
        assert!(phases.limit(Duration::from_secs(35)).unwrap() <= Duration::from_secs(1));
        budget.cancel();
        let mut called = false;
        assert!(phases
            .during(|| {
                called = true;
                Ok(())
            })
            .is_err());
        assert!(!called);
        assert!(phases.limit(Duration::from_secs(35)).is_err());
    }

    #[test]
    fn filesystem_effect_survives_late_fence_but_cannot_certify_success() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("candidate");
        let budget = FinalizationBudget::begin(Duration::from_secs(1)).unwrap();
        let phases = FinalizationPhases(Some(budget.clone()));
        assert!(phases
            .during(|| {
                fs::write(&path, b"candidate")?;
                File::open(&path)?.sync_all()?;
                budget.cancel();
                Ok(())
            })
            .is_err());
        assert_eq!(fs::read(path).unwrap(), b"candidate");
        assert!(phases.check().is_err());
    }

    #[test]
    fn real_stalled_wait_consumes_original_budget_and_blocks_next_phase() {
        let budget = FinalizationBudget::begin(Duration::from_millis(60)).unwrap();
        let phases = FinalizationPhases(Some(budget.clone()));
        let original = budget.constraint();
        let (mut reader, _held_writer) = UnixStream::pair().unwrap();
        let began = Instant::now();
        // Socket waits use the same cap as RPC close/freeze. Early interrupted
        // waits must sample again; none may restart the deadline.
        while phases.check().is_ok() {
            let result = phases.during(|| {
                reader.set_read_timeout(Some(phases.limit(Duration::from_secs(35))?))?;
                reader.read_exact(&mut [0])?;
                Ok(())
            });
            assert!(result.is_err());
            assert!(began.elapsed() < Duration::from_secs(2));
        }
        assert_eq!(original, budget.constraint());
        assert!(phases.limit(Duration::from_secs(35)).is_err());
        let mut next = false;
        assert!(phases
            .during(|| {
                next = true;
                Ok(())
            })
            .is_err());
        assert!(!next);
    }
}
