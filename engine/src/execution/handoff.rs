//! One-use sealed-spec descriptor transfer after portable-pty has closed all
//! inherited FDs. Socket names are not credentials: kernel peer PID/UID plus
//! held pidfds authenticate both ends. No JSON spec enters argv/env/PTY/disk.
use super::*;
use super::finalization_budget::{FinalizationBudget, FINALIZATION_POLICY_LIMIT};
use crate::protocol::ObjectKind;
use anyhow::{bail, Context};
use std::{
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    sync::mpsc,
    time::{Duration, Instant},
};

/// The actual controller process authenticated by a successful managed handoff.
/// Construction stays private to `receive_managed`: a saved PID, pathname, raw
/// descriptor or serialized context cannot recreate this witness.
///
/// This proves kernel process lifetime and exact socket peer identity only. It
/// does not grant owner/run authority or prove a held engine lease. Keep it on
/// the trusted helper/supervisor side; never project it or its pidfd into the
/// guest. CLOEXEC does not replace the child FD allowlist or protection before
/// a same-UID guest can inspect the trusted process through `/proc`.
pub struct ControllerProcessWitness {
    lifetime: OwnedFd,
    pid: u32,
}
impl ControllerProcessWitness {
    pub fn check_current(&self) -> Result<()> {
        ensure!(
            !exited(&self.lifetime)?,
            "managed controller process exited"
        );
        Ok(())
    }
    pub fn verify_peer(&self, stream: &UnixStream) -> Result<()> {
        self.check_current()?;
        let actual = peer(stream)?;
        ensure!(
            actual.pid == self.pid as i32 && actual.uid == unsafe { libc::geteuid() },
            "controller socket does not belong to the authenticated parent"
        );
        self.check_current()
    }
}

/// A kernel lifetime witness received only through an owned helper FD. It has
/// no Deserialize implementation; a saved PID or guest JSON cannot recreate it.
pub struct BackendProcessWitness {
    pidfd: OwnedFd,
    pid: u32,
    run_id: String,
    execution_id: String,
    execution_profile: String,
    frontend_start: ManagedFrontendStart,
    network: Option<super::network_phase::OwnedNetworkReady>,
}
impl BackendProcessWitness {
    fn received(pidfd: OwnedFd, binding: &ManagedExecutionBinding) -> Result<Self> {
        ensure!(
            binding.format_version == 2 && binding.profile.is_none(),
            "legacy readiness cannot authorize a v4 backend"
        );
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
        let pid: u32 = info
            .lines()
            .find_map(|line| line.strip_prefix("Pid:\t"))
            .context("helper did not transfer a live pidfd")?
            .parse()?;
        ensure!(pid > 1 && !exited(&pidfd)?, "owned backend already exited");
        Ok(Self {
            pidfd,
            pid,
            run_id: binding.run_id.clone(),
            execution_id: binding.execution_id.clone(),
            execution_profile: binding.execution_profile.clone(),
            frontend_start: binding.frontend_start,
            network: None,
        })
    }
    fn received_network(
        mut descriptors: Vec<OwnedFd>,
        binding: &ManagedExecutionBinding,
        request_digest: &str,
        frame: super::network_phase::NetworkReadyFrame,
    ) -> Result<Self> {
        ensure!(
            descriptors.len() == 3,
            "v4 backend readiness requires pidfd and USER/NET descriptors"
        );
        validate_network_frame(binding, request_digest, &frame)?;
        let pidfd = descriptors.remove(0);
        let pid = super::network_phase::live_pid(&pidfd)?;
        let network = super::network_phase::OwnedNetworkReady::receive_checked(
            frame,
            &pidfd,
            pid,
            descriptors,
        )?;
        network.validate_backend(&pidfd)?;
        Ok(Self {
            pidfd,
            pid,
            run_id: binding.run_id.clone(),
            execution_id: binding.execution_id.clone(),
            execution_profile: binding.execution_profile.clone(),
            frontend_start: binding.frontend_start,
            network: Some(network),
        })
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn matches_attempt(&self, run: &str, attempt: &str, profile: &str) -> bool {
        self.run_id == run && self.execution_id == attempt && self.execution_profile == profile
    }
    pub fn authenticated_start_required(&self) -> bool {
        self.frontend_start == ManagedFrontendStart::AfterHostApiKeyV1
    }
    pub fn check_current(&self) -> Result<()> {
        ensure!(!exited(&self.pidfd)?, "owned backend process exited");
        if let Some(network) = &self.network {
            network.validate_backend(&self.pidfd)?;
        }
        Ok(())
    }
    pub fn verify_peer(&self, stream: &UnixStream) -> Result<()> {
        self.check_current()?;
        let actual = peer(stream)?;
        ensure!(
            actual.pid == self.pid as i32 && actual.uid == unsafe { libc::geteuid() },
            "backend socket does not belong to the helper-owned process"
        );
        self.check_current()
    }
}

pub enum ManagedExecutionEvent {
    BackendReady { process: Arc<BackendProcessWitness> },
    Shutdown(ValidatedManagedShutdown),
    Uncertain,
}
/// Constructed only by the owned FD receiver after exact receipt/process checks.
/// Historical wire JSON cannot deserialize into this event capability.
pub struct ValidatedManagedShutdown {
    receipt: ManagedShutdownReceipt,
}
impl ValidatedManagedShutdown {
    pub fn request(&self) -> &StopAndCheckpoint {
        &self.receipt.request
    }
    pub fn record(&self) -> &ExecutionRecord {
        &self.receipt.record
    }
}
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum ControlEvent {
    BackendReady {
        protocol_version: u32,
        frontend_start: ManagedFrontendStart,
        execution_id: String,
        operation_id: String,
        view_id: String,
    },
    BackendReadyV3 {
        protocol_version: u32,
        frontend_start: ManagedFrontendStart,
        execution_id: String,
        operation_id: String,
        view_id: String,
        network: super::network_phase::NetworkReadyFrame,
        partial_binding_digest: Option<String>,
    },
    Shutdown {
        receipt: ManagedShutdownReceipt,
    },
}
impl<'de> Deserialize<'de> for ControlEvent {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Internally tagged derives buffer nested values in Serde's Content.
        // Record codecs need the original JSON to preserve duplicate fields and
        // integer tokens. Dispatch on raw bytes, then decode the full strict
        // struct from those same bytes; never normalize this through Value.
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        #[derive(Deserialize)]
        struct Tag {
            event: String,
        }
        let tag: Tag = serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
        match tag.event.as_str() {
            "backend_ready" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Ready {
                    #[serde(rename = "event")]
                    _event: String,
                    protocol_version: u32,
                    frontend_start: ManagedFrontendStart,
                    execution_id: String,
                    operation_id: String,
                    view_id: String,
                }
                let value: Ready =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                Ok(Self::BackendReady {
                    protocol_version: value.protocol_version,
                    frontend_start: value.frontend_start,
                    execution_id: value.execution_id,
                    operation_id: value.operation_id,
                    view_id: value.view_id,
                })
            }
            "backend_ready_v3" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Ready {
                    #[serde(rename = "event")]
                    _event: String,
                    protocol_version: u32,
                    frontend_start: ManagedFrontendStart,
                    execution_id: String,
                    operation_id: String,
                    view_id: String,
                    network: super::network_phase::NetworkReadyFrame,
                    partial_binding_digest: Option<String>,
                }
                let value: Ready =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                Ok(Self::BackendReadyV3 {
                    protocol_version: value.protocol_version,
                    frontend_start: value.frontend_start,
                    execution_id: value.execution_id,
                    operation_id: value.operation_id,
                    view_id: value.view_id,
                    network: value.network,
                    partial_binding_digest: value.partial_binding_digest,
                })
            }
            "shutdown" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Shutdown {
                    #[serde(rename = "event")]
                    _event: String,
                    receipt: ManagedShutdownReceipt,
                }
                let value: Shutdown =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                Ok(Self::Shutdown {
                    receipt: value.receipt,
                })
            }
            _ => Err(serde::de::Error::custom("unknown managed control event")),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum ParentCommand {
    Stop { request: StopAndCheckpoint },
    BackendAuthenticated { request: StopAndCheckpoint },
}

/// App operations only enqueue/read bounded typed messages. IO and process
/// lifetime checks run on its bounded owned worker, never the App thread.
pub struct ManagedExecutionHandle {
    binding: ManagedExecutionBinding,
    commands: mpsc::SyncSender<ParentCommand>,
    events: mpsc::Receiver<ManagedExecutionEvent>,
    pending: std::cell::RefCell<Option<StopAndCheckpoint>>,
    authenticated: std::cell::Cell<bool>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}
impl ManagedExecutionHandle {
    pub fn stop_and_checkpoint(&self, request: StopAndCheckpoint) -> Result<()> {
        request.validate(&self.binding)?;
        ensure!(
            !self.closed.load(std::sync::atomic::Ordering::Acquire),
            "managed execution control closed"
        );
        let mut pending = self.pending.borrow_mut();
        if let Some(previous) = pending.as_ref() {
            ensure!(
                *previous == request,
                "managed shutdown already has another request"
            );
            return Ok(());
        }
        self.commands
            .try_send(ParentCommand::Stop {
                request: request.clone(),
            })
            .context("managed shutdown control admission failed")?;
        *pending = Some(request);
        Ok(())
    }
    pub fn try_event(&self) -> Option<ManagedExecutionEvent> {
        self.events.try_recv().ok()
    }
    pub fn backend_authenticated(&self) -> Result<()> {
        ensure!(
            self.binding.frontend_start == ManagedFrontendStart::AfterHostApiKeyV1
                && self.pending.borrow().is_none()
                && !self.closed.load(std::sync::atomic::Ordering::Acquire),
            "managed frontend authentication is no longer admissible"
        );
        if self.authenticated.get() {
            return Ok(());
        }
        let request = StopAndCheckpoint {
            request_id: self.binding.request_id.clone(),
            execution_id: self.binding.execution_id.clone(),
            operation_id: self.binding.operation_id.clone(),
            view_id: self.binding.view_id.clone(),
            run_id: self.binding.run_id.clone(),
            run_epoch: self.binding.run_epoch,
            owner: self.binding.owner.clone(),
        };
        self.commands
            .try_send(ParentCommand::BackendAuthenticated { request })
            .context("managed frontend authentication admission failed")?;
        self.authenticated.set(true);
        Ok(())
    }
}
impl Drop for ManagedExecutionHandle {
    fn drop(&mut self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

pub struct ManagedExecutionHandoff {
    handoff: ExecutionHandoff,
    binding: ManagedExecutionBinding,
    binding_fd: OwnedFd,
    parent_control: UnixStream,
    helper_control: UnixStream,
    request_digest: String,
}
impl ManagedExecutionHandoff {
    pub fn binding(&self) -> &ManagedExecutionBinding {
        &self.binding
    }
    pub fn new(spec: &ExecutionSpec, binding: ManagedExecutionBinding) -> Result<Self> {
        use sha2::{Digest, Sha256};
        binding.validate(spec)?;
        let request_digest = hex::encode(Sha256::digest(crate::protocol::canonical_json(spec)?));
        let handoff = ExecutionHandoff::new(spec)?;
        let binding_fd =
            super::linux::sealed_bytes_fd(&crate::protocol::canonical_json(&binding)?)?;
        let (parent_control, helper_control) = UnixStream::pair()?;
        Ok(Self {
            handoff,
            binding,
            binding_fd,
            parent_control,
            helper_control,
            request_digest,
        })
    }
    pub fn helper_args(&self) -> Vec<String> {
        let mut args = self.handoff.helper_args();
        args[2] = "managed-helper".into();
        args
    }
    pub fn activate(self, child: u32) -> Result<(HandoffReceipt, ManagedExecutionHandle)> {
        ensure!(
            child > 1 && child != self.handoff.parent,
            "invalid managed handoff child"
        );
        let lifetime = pidfd(child)?;
        let Self {
            handoff,
            binding,
            binding_fd,
            parent_control,
            helper_control,
            request_digest,
        } = self;
        let (commands, receive_commands) = mpsc::sync_channel(2);
        let (send_events, events) = mpsc::sync_channel(4);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = ManagedExecutionHandle {
            binding: binding.clone(),
            commands,
            events,
            pending: std::cell::RefCell::new(None),
            authenticated: std::cell::Cell::new(false),
            closed: closed.clone(),
        };
        let (send, result) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("rudder-managed-control".into())
            .spawn(move || {
                let ready = (|| -> Result<()> {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let mut rejected = 0;
                    loop {
                        ensure!(
                            Instant::now() < deadline && !exited(&lifetime)?,
                            "managed handoff deadline/lifetime ended"
                        );
                        match handoff.listener.accept() {
                            Ok((stream, _)) => {
                                let credentials = peer(&stream)?;
                                if credentials.uid != handoff.uid || credentials.pid != child as i32
                                {
                                    rejected += 1;
                                    ensure!(
                                        rejected <= 32,
                                        "too many rejected managed handoff peers"
                                    );
                                    continue;
                                }
                                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                                send_descriptors(
                                    &stream,
                                    &[
                                        handoff.spec.as_raw_fd(),
                                        binding_fd.as_raw_fd(),
                                        helper_control.as_raw_fd(),
                                    ],
                                )?;
                                return Ok(());
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                })();
                let success = ready.is_ok();
                let _ = send.send(ready.map_err(|_| "managed descriptor handoff failed".into()));
                drop(handoff);
                drop(binding_fd);
                drop(helper_control);
                if !success {
                    let _ = send_events.try_send(ManagedExecutionEvent::Uncertain);
                    return;
                }
                let run = managed_client_loop(
                    parent_control,
                    &binding,
                    &request_digest,
                    child,
                    &lifetime,
                    receive_commands,
                    &send_events,
                    &closed,
                );
                if run.is_err() {
                    let _ = send_events.try_send(ManagedExecutionEvent::Uncertain);
                }
            })?;
        Ok((
            HandoffReceipt {
                result,
                completed: std::cell::Cell::new(false),
            },
            handle,
        ))
    }
}

fn managed_client_loop(
    mut stream: UnixStream,
    binding: &ManagedExecutionBinding,
    request_digest: &str,
    helper_pid: u32,
    lifetime: &OwnedFd,
    commands: mpsc::Receiver<ParentCommand>,
    events: &mpsc::SyncSender<ManagedExecutionEvent>,
    closed: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut pending: Option<StopAndCheckpoint> = None;
    let mut backend: Option<Arc<BackendProcessWitness>> = None;
    loop {
        ensure!(
            !closed.load(std::sync::atomic::Ordering::Acquire),
            "managed control retired"
        );
        if let Ok(command) = commands.try_recv() {
            match &command {
                ParentCommand::Stop { request } => {
                    request.validate(binding)?;
                    ensure!(pending.is_none(), "duplicate managed shutdown write");
                    pending = Some(request.clone());
                }
                ParentCommand::BackendAuthenticated { request } => {
                    request.validate(binding)?;
                    ensure!(
                        pending.is_none()
                            && request.request_id == binding.request_id
                            && backend.is_some()
                            && binding.frontend_start == ManagedFrontendStart::AfterHostApiKeyV1,
                        "managed frontend authentication binding changed"
                    );
                }
            }
            send_control_packet(&mut stream, &command, None)?;
        }
        if packet_available(&stream)? {
            let (event, mut descriptors): (ControlEvent, _) =
                receive_control_packet_fds(&mut stream)?;
            match event {
                ControlEvent::BackendReady {
                    protocol_version,
                    frontend_start,
                    execution_id,
                    operation_id,
                    view_id,
                } => {
                    ensure!(
                        binding.format_version == 2
                            && descriptors.len() == 1
                            && protocol_version == 2
                            && frontend_start == binding.frontend_start
                            && backend.is_none()
                            && execution_id == binding.execution_id
                            && operation_id == binding.operation_id
                            && view_id == binding.view_id,
                        "managed backend readiness binding changed"
                    );
                    let process = Arc::new(BackendProcessWitness::received(
                        descriptors.pop().unwrap(),
                        binding,
                    )?);
                    events
                        .try_send(ManagedExecutionEvent::BackendReady {
                            process: process.clone(),
                        })
                        .context("managed readiness queue full")?;
                    backend = Some(process);
                }
                ControlEvent::BackendReadyV3 {
                    protocol_version,
                    frontend_start,
                    execution_id,
                    operation_id,
                    view_id,
                    network,
                    partial_binding_digest,
                } => {
                    ensure!(
                        binding.format_version == 3
                            && protocol_version == 3
                            && frontend_start == binding.frontend_start
                            && backend.is_none()
                            && execution_id == binding.execution_id
                            && operation_id == binding.operation_id
                            && view_id == binding.view_id,
                        "v4 backend readiness differs from the sealed assignment"
                    );
                    let expected = binding
                        .profile
                        .as_ref()
                        .context("v4 backend lacks profile")?
                        .partial_binding()?
                        .map(|binding| binding.digest())
                        .transpose()?;
                    ensure!(
                        partial_binding_digest == expected,
                        "v4 backend readiness changed workspace protocol"
                    );
                    let process = Arc::new(BackendProcessWitness::received_network(
                        descriptors,
                        binding,
                        request_digest,
                        network,
                    )?);
                    events
                        .try_send(ManagedExecutionEvent::BackendReady {
                            process: process.clone(),
                        })
                        .context("managed readiness queue full")?;
                    backend = Some(process);
                }
                ControlEvent::Shutdown { receipt } => {
                    ensure!(
                        descriptors.is_empty(),
                        "shutdown receipt supplied unexpected descriptors"
                    );
                    receipt.request.validate(binding)?;
                    ensure!(
                        pending.as_ref() == Some(&receipt.request)
                            && receipt.kernel_drained
                            && receipt.record.request_digest() == request_digest
                            && receipt.record.supervisor_pid() == helper_pid
                            && receipt.record.agent_exit().is_some()
                            && receipt.record.execution_id() == binding.execution_id
                            && receipt.record.operation_id() == binding.operation_id
                            && receipt.record.view_id() == binding.view_id
                            && receipt.record.owner() == &binding.owner
                            && receipt.record.phase() == ExecutionPhase::Checkpointed,
                        "managed shutdown receipt is unproved or belongs to another attempt"
                    );
                    validate_shutdown_artifact(binding, request_digest, &receipt)?;
                    if binding.format_version == 3 {
                        ensure!(
                            backend
                                .as_ref()
                                .is_some_and(|process| process.network.is_some()),
                            "v4 shutdown has no actual backend network readiness"
                        );
                    }
                    ensure!(
                        backend
                            .as_ref()
                            .map(|process| exited(&process.pidfd))
                            .transpose()?
                            .unwrap_or(true),
                        "managed shutdown receipt arrived while the owned backend is alive"
                    );
                    events
                        .try_send(ManagedExecutionEvent::Shutdown(ValidatedManagedShutdown {
                            receipt,
                        }))
                        .context("managed receipt queue full")?;
                    return Ok(());
                }
            }
        }
        if exited(lifetime)? {
            // A final packet may arrive between the first poll and pidfd exit.
            // Drain it before concluding that physical exit had no receipt.
            ensure!(
                packet_available(&stream)?,
                "managed helper exited without a shutdown receipt"
            );
            continue;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn validate_network_frame(
    binding: &ManagedExecutionBinding,
    request_digest: &str,
    frame: &super::network_phase::NetworkReadyFrame,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let profile = binding
        .profile
        .as_ref()
        .context("v4 readiness lacks sealed network profile")?;
    ensure!(
        binding.format_version == 3
            && frame.format_version == 1
            && profile.network.mode == super::network::NetworkMode::OfflineV1
            && frame.execution_id == binding.execution_id
            && frame.execution_request_digest == request_digest
            && frame.network_id == profile.network.binding.network_id
            && frame.readiness_nonce == profile.network.readiness_nonce
            && frame.network_profile_digest
                == hex::encode(Sha256::digest(serde_json::to_vec(&profile.network)?))
            && Uuid::parse_str(&frame.child_nonce)
                .is_ok_and(|id| id.to_string() == frame.child_nonce),
        "v4 readiness changed its sealed network assignment"
    );
    Ok(())
}
fn validate_local_checkpoint(checkpoint: &VfsCheckpoint) -> Result<()> {
    checkpoint.directory.validate()?;
    checkpoint.hardlinks.validate()?;
    ensure!(
        checkpoint.format_version == 1
            && checkpoint.directory.kind == ObjectKind::Directory
            && checkpoint.hardlinks.kind == ObjectKind::Blob,
        "managed shutdown checkpoint has invalid reference kinds"
    );
    Ok(())
}
fn validate_shutdown_artifact(
    binding: &ManagedExecutionBinding,
    request_digest: &str,
    receipt: &ManagedShutdownReceipt,
) -> Result<()> {
    if binding.format_version == 2 {
        ensure!(
            binding.profile.is_none() && receipt.record.format_version() == 1,
            "legacy shutdown cannot carry a new execution artifact"
        );
        let Some(CheckpointRef::CompleteLocal(checkpoint)) = receipt.record.checkpoint() else {
            anyhow::bail!("legacy managed shutdown requires its complete local checkpoint");
        };
        return validate_local_checkpoint(checkpoint);
    }
    ensure!(
        binding.format_version == 3,
        "unknown managed shutdown protocol"
    );
    let profile = binding
        .profile
        .as_ref()
        .context("v4 shutdown lacks sealed profile")?;
    let ExecutionRecord::V2(record) = &receipt.record else {
        anyhow::bail!("v4 shutdown requires record2")
    };
    receipt.record.validate_metadata()?;
    ensure!(
        record.identity.project_id == profile.network.binding.project_id
            && record.identity.request_id == binding.request_id
            && record.identity.run_id == binding.run_id
            && record.identity.run_epoch == binding.run_epoch,
        "record2 shutdown changed project/request/run identity"
    );
    match (&profile.workspace, receipt.record.checkpoint()) {
        (
            ExecutionWorkspaceSelectionV4::LocalComplete {},
            Some(CheckpointRef::CompleteLocal(checkpoint)),
        ) => validate_local_checkpoint(checkpoint),
        (
            ExecutionWorkspaceSelectionV4::RemoteSourceOnly { selection, .. },
            Some(CheckpointRef::PartialRemote(checkpoint)),
        ) => {
            ensure!(
                checkpoint.stop == receipt.request
                    && checkpoint.execution_request_digest == request_digest
                    && checkpoint.observation == selection.observation
                    && checkpoint.capture.base == selection.root
                    && checkpoint.capture.attempt == selection.attempt,
                "partial shutdown changed source selection or stop"
            );
            Ok(())
        }
        _ => anyhow::bail!("managed shutdown changed its sealed checkpoint family"),
    }
}

pub struct ManagedControl {
    pub binding: ManagedExecutionBinding,
    controller: Arc<ControllerProcessWitness>,
    finalization: Option<FinalizationBudget>,
    stream: UnixStream,
    pub(crate) request: Option<StopAndCheckpoint>,
    pub(crate) authenticated: bool,
}
impl ManagedControl {
    /// Clone the held controller identity from the successful authenticated
    /// handoff. Only trusted host services may retain this witness. It remains
    /// a process witness after control close, never a current owner/run grant.
    pub fn controller_process(&self) -> Arc<ControllerProcessWitness> {
        Arc::clone(&self.controller)
    }
    /// One helper-observed deadline after the first authenticated legacy stop.
    /// Repeated packets never renew it. Legacy wire bytes cannot account for
    /// queue time before reception; the new stop family must carry the original
    /// parent's constraint. This value supplies time bounds, not drain proof.
    pub(crate) fn finalization_budget(&self) -> Option<FinalizationBudget> {
        self.finalization.clone()
    }
    pub(crate) fn poll_stop(&mut self) -> Result<bool> {
        if packet_available(&self.stream)? {
            let (command, descriptor): (ParentCommand, _) =
                receive_control_packet(&mut self.stream)?;
            ensure!(
                descriptor.is_none(),
                "shutdown request supplied a descriptor"
            );
            match command {
                ParentCommand::Stop { request } => {
                    request.validate(&self.binding)?;
                    ensure!(
                        self.request.as_ref().is_none_or(|prior| *prior == request),
                        "shutdown request changed"
                    );
                    if self.request.is_none() {
                        self.finalization = Some(FinalizationBudget::begin(FINALIZATION_POLICY_LIMIT)?);
                    }
                    self.request = Some(request);
                }
                ParentCommand::BackendAuthenticated { request } => {
                    request.validate(&self.binding)?;
                    ensure!(
                        self.request.is_none()
                            && request.request_id == self.binding.request_id
                            && self.binding.frontend_start
                                == ManagedFrontendStart::AfterHostApiKeyV1,
                        "frontend authentication belongs to another launch"
                    );
                    self.authenticated = true;
                }
            }
        }
        Ok(self.request.is_some())
    }
    pub(crate) fn backend_ready(&mut self, process: &OwnedFd) -> Result<()> {
        ensure!(
            self.binding.format_version == 2 && self.binding.profile.is_none(),
            "legacy readiness cannot authorize a v4 backend"
        );
        send_control_packet(
            &mut self.stream,
            &ControlEvent::BackendReady {
                protocol_version: 2,
                frontend_start: self.binding.frontend_start,
                execution_id: self.binding.execution_id.clone(),
                operation_id: self.binding.operation_id.clone(),
                view_id: self.binding.view_id.clone(),
            },
            Some(process),
        )
    }
    pub(super) fn backend_ready_network(
        &mut self,
        process: &OwnedFd,
        network: &super::network_phase::OwnedNetworkReady,
    ) -> Result<()> {
        validate_network_frame(
            &self.binding,
            &network.frame().execution_request_digest,
            network.frame(),
        )?;
        network.validate_backend(process)?;
        let partial_binding_digest = self
            .binding
            .profile
            .as_ref()
            .context("v4 backend profile absent")?
            .partial_binding()?
            .map(|binding| binding.digest())
            .transpose()?;
        let ns = network.descriptors();
        send_control_packet_fds(
            &mut self.stream,
            &ControlEvent::BackendReadyV3 {
                protocol_version: 3,
                frontend_start: self.binding.frontend_start,
                execution_id: self.binding.execution_id.clone(),
                operation_id: self.binding.operation_id.clone(),
                view_id: self.binding.view_id.clone(),
                network: network.frame().clone(),
                partial_binding_digest,
            },
            &[process.as_raw_fd(), ns[0], ns[1]],
        )
    }
    pub(super) fn shutdown_receipt(
        &mut self,
        committed: &CommittedExecutionRecord,
        drain: &super::linux::OwnedExecutionDrain,
    ) -> Result<()> {
        drain.validate_publication(committed)?;
        if let Some(request) = &self.request {
            drain.validate_stop(request)?;
            send_control_packet(
                &mut self.stream,
                &ControlEvent::Shutdown {
                    receipt: ManagedShutdownReceipt {
                        request: request.clone(),
                        record: committed.record().clone(),
                        kernel_drained: true,
                    },
                },
                None,
            )?;
        }
        Ok(())
    }
}

fn packet_available(stream: &UnixStream) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut poll, 1, 0) };
    ensure!(result >= 0, "managed control poll failed");
    if poll.revents & libc::POLLIN != 0 {
        return Ok(true);
    }
    ensure!(
        poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0,
        "managed controller disconnected"
    );
    Ok(false)
}
pub(crate) fn send_control_packet(
    stream: &mut UnixStream,
    value: &impl Serialize,
    descriptor: Option<&OwnedFd>,
) -> Result<()> {
    let descriptors: Vec<i32> = descriptor.into_iter().map(AsRawFd::as_raw_fd).collect();
    send_control_packet_fds(stream, value, &descriptors)
}
pub(crate) fn send_control_packet_fds(
    stream: &mut UnixStream,
    value: &impl Serialize,
    descriptors: &[i32],
) -> Result<()> {
    let bytes = crate::protocol::canonical_json(value)?;
    ensure!(
        bytes.len() <= 64 * 1024,
        "managed control frame exceeds bound"
    );
    if !descriptors.is_empty() {
        send_descriptors(stream, descriptors)?;
    } else {
        stream.write_all(&[0])?;
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}
pub(crate) fn receive_control_packet<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<(T, Option<OwnedFd>)> {
    let (value, mut descriptors) = receive_control_packet_fds(stream)?;
    ensure!(
        descriptors.len() <= 1,
        "legacy managed control received extra descriptors"
    );
    Ok((value, descriptors.pop()))
}
pub(crate) fn receive_control_packet_fds<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<(T, Vec<OwnedFd>)> {
    let (tag, descriptors) = receive_descriptors(stream)?;
    ensure!(
        (tag == 0 && descriptors.is_empty())
            || (tag == 1 && !descriptors.is_empty() && descriptors.len() <= 3),
        "invalid managed control descriptor frame"
    );
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(
        size > 0 && size <= 64 * 1024,
        "managed control frame exceeds bound"
    );
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Ok((serde_json::from_slice(&bytes)?, descriptors))
}

pub struct ExecutionHandoff {
    listener: UnixListener,
    spec: OwnedFd,
    name: String,
    parent: u32,
    uid: u32,
}
pub struct HandoffReceipt {
    result: mpsc::Receiver<std::result::Result<(), String>>,
    completed: std::cell::Cell<bool>,
}
impl HandoffReceipt {
    /// Reports descriptor transfer only, not helper validation, process launch,
    /// namespace readiness or successful execution. Observe ExecutionRecord.
    pub fn try_result(&self) -> Option<std::result::Result<(), String>> {
        if self.completed.get() {
            return None;
        }
        match self.result.try_recv() {
            Ok(r) => {
                self.completed.set(true);
                Some(r)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.completed.set(true);
                Some(Err(
                    "execution handoff closed without acknowledgement".into()
                ))
            }
        }
    }
    pub fn wait(self, timeout: Duration) -> Result<()> {
        self.result
            .recv_timeout(timeout)
            .context("execution handoff deadline")?
            .map_err(anyhow::Error::msg)
    }
}
impl ExecutionHandoff {
    pub fn new(spec: &ExecutionSpec) -> Result<Self> {
        let fd = super::linux::sealed_spec_fd(spec)?;
        let name = format!("rudder-execution-{}", Uuid::new_v4());
        let address = SocketAddr::from_abstract_name(name.as_bytes())?;
        let listener = UnixListener::bind_addr(&address)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            spec: fd,
            name,
            parent: std::process::id(),
            uid: unsafe { libc::geteuid() },
        })
    }
    /// Only non-secret routing metadata. Use with the real native helper path.
    pub fn helper_args(&self) -> Vec<String> {
        vec![
            "fabric".into(),
            "execution".into(),
            "helper".into(),
            "--handoff".into(),
            self.name.clone(),
            "--parent-pid".into(),
            self.parent.to_string(),
        ]
    }
    /// Call immediately after TerminalPane owns the actual spawned process.
    pub fn activate(self, child: u32) -> Result<HandoffReceipt> {
        ensure!(child > 1 && child != self.parent, "invalid handoff child");
        let pidfd = pidfd(child)?;
        let (send, result) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let r = (|| -> Result<()> {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut rejected = 0;
                while Instant::now() < deadline {
                    ensure!(
                        !exited(&pidfd)?,
                        "execution child exited before spec handoff"
                    );
                    match self.listener.accept() {
                        Ok((stream, _)) => {
                            let credentials = peer(&stream)?;
                            if credentials.uid != self.uid || credentials.pid != child as i32 {
                                rejected += 1;
                                ensure!(
                                    rejected <= 32,
                                    "too many rejected execution handoff peers"
                                );
                                continue;
                            }
                            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                            send_fd(&stream, &self.spec)?;
                            return Ok(());
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                bail!("execution child did not receive spec before deadline")
            })();
            drop(self);
            let _ = send.send(r.map_err(|e| e.to_string()));
        });
        Ok(HandoffReceipt {
            result,
            completed: std::cell::Cell::new(false),
        })
    }
}
pub(crate) fn pidfd(pid: u32) -> Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("open execution lifetime pidfd");
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
fn exited(fd: &OwnedFd) -> Result<bool> {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut p, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(result > 0)
}
fn peer(stream: &UnixStream) -> Result<libc::ucred> {
    let mut value = unsafe { mem::zeroed::<libc::ucred>() };
    let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut value as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    ensure!(
        length as usize == mem::size_of::<libc::ucred>(),
        "invalid handoff peer credentials"
    );
    Ok(value)
}
fn send_fd(stream: &UnixStream, fd: &OwnedFd) -> Result<()> {
    send_descriptors(stream, &[fd.as_raw_fd()])
}
fn send_descriptors(stream: &UnixStream, descriptors: &[i32]) -> Result<()> {
    ensure!(
        !descriptors.is_empty() && descriptors.len() <= 3,
        "invalid execution descriptor count"
    );
    let mut byte = [1u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 4];
    let mut header = unsafe { mem::zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    let descriptor_bytes = mem::size_of_val(descriptors);
    header.msg_controllen = unsafe { libc::CMSG_SPACE(descriptor_bytes as u32) } as usize;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&header);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(descriptor_bytes as u32) as usize;
        for (index, descriptor) in descriptors.iter().enumerate() {
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>().add(index), *descriptor);
        }
    }
    ensure!(
        unsafe { libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL) } == 1,
        "sealed execution descriptor transfer failed"
    );
    Ok(())
}
pub fn receive(name: &str, parent: u32) -> Result<ExecutionSpec> {
    let _parent_fence = super::linux::arm_parent(parent)?;
    ensure!(
        name.starts_with("rudder-execution-")
            && name.len() <= 80
            && parent > 1
            && unsafe { libc::getppid() } == parent as i32,
        "execution handoff parent/routing mismatch"
    );
    let lifetime = pidfd(parent)?;
    ensure!(!exited(&lifetime)?, "execution parent already exited");
    let address = SocketAddr::from_abstract_name(name.as_bytes())?;
    let stream = UnixStream::connect_addr(&address)?;
    stream.set_read_timeout(Some(Duration::from_secs(12)))?;
    let cred = peer(&stream)?;
    ensure!(
        cred.uid == unsafe { libc::geteuid() } && cred.pid == parent as i32,
        "execution handoff server is not the owning parent"
    );
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 4];
    let mut header = unsafe { mem::zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = mem::size_of_val(&control);
    let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
    let mut descriptors = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&header);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let n = ((*c).cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize))
                    / mem::size_of::<i32>();
                for index in 0..n {
                    descriptors.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                        libc::CMSG_DATA(c).cast::<i32>().add(index),
                    )));
                }
            }
            c = libc::CMSG_NXTHDR(&header, c);
        }
    }
    ensure!(
        count == 1
            && byte[0] == 1
            && header.msg_flags & libc::MSG_CTRUNC == 0
            && descriptors.len() == 1,
        "invalid sealed execution handoff frame"
    );
    ensure!(
        !exited(&lifetime)?,
        "execution parent exited during handoff"
    );
    super::linux::read_spec_fd(descriptors.pop().unwrap())
}

fn receive_descriptors(stream: &UnixStream) -> Result<(u8, Vec<OwnedFd>)> {
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut ancillary = [0u64; 4];
    let mut header = unsafe { mem::zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = ancillary.as_mut_ptr().cast();
    header.msg_controllen = mem::size_of_val(&ancillary);
    let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
    let mut descriptors = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&header);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize))
                    / mem::size_of::<i32>();
                for index in 0..count {
                    descriptors.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                        libc::CMSG_DATA(c).cast::<i32>().add(index),
                    )));
                }
            }
            c = libc::CMSG_NXTHDR(&header, c);
        }
    }
    ensure!(
        count == 1 && header.msg_flags & libc::MSG_CTRUNC == 0,
        "managed descriptor channel closed or truncated"
    );
    Ok((byte[0], descriptors))
}

pub fn receive_managed(name: &str, parent: u32) -> Result<(ExecutionSpec, ManagedControl)> {
    let _parent_fence = super::linux::arm_parent(parent)?;
    ensure!(
        name.starts_with("rudder-execution-")
            && name.len() <= 80
            && parent > 1
            && unsafe { libc::getppid() } == parent as i32,
        "managed handoff parent mismatch"
    );
    let lifetime = pidfd(parent)?;
    let stream = UnixStream::connect_addr(&SocketAddr::from_abstract_name(name.as_bytes())?)?;
    stream.set_read_timeout(Some(Duration::from_secs(12)))?;
    let actual = peer(&stream)?;
    ensure!(
        actual.pid == parent as i32 && actual.uid == unsafe { libc::geteuid() },
        "managed handoff server is not its owning parent"
    );
    let (tag, mut descriptors) = receive_descriptors(&stream)?;
    ensure!(
        tag == 1 && descriptors.len() == 3 && !exited(&lifetime)?,
        "invalid managed handoff descriptors/lifetime"
    );
    let control = UnixStream::from(descriptors.pop().unwrap());
    control.set_read_timeout(Some(Duration::from_secs(2)))?;
    control.set_write_timeout(Some(Duration::from_secs(2)))?;
    let binding: ManagedExecutionBinding = serde_json::from_slice(
        &super::linux::read_sealed_bytes(descriptors.pop().unwrap())?,
    )?;
    let spec = super::linux::read_spec_fd(descriptors.pop().unwrap())?;
    binding.validate(&spec)?;
    // Retain this already-open kernel identity, not a PID reopened from saved
    // binding data. No additional descriptor is transferred over the wire.
    let controller = Arc::new(ControllerProcessWitness {
        lifetime,
        pid: actual.pid as u32,
    });
    controller.verify_peer(&stream)?;
    Ok((
        spec,
        ManagedControl {
            binding,
            controller,
            finalization: None,
            stream: control,
            request: None,
            authenticated: false,
        },
    ))
}

#[cfg(test)]
mod managed_tests {
    use super::*;
    // Test-only construction for isolated control-packet fixtures. Production
    // construction occurs solely after receive_managed authenticates the peer,
    // three descriptors, sealed spec and exact managed binding.
    fn controller_test_fixture() -> Arc<ControllerProcessWitness> {
        Arc::new(ControllerProcessWitness {
            lifetime: pidfd(std::process::id()).unwrap(),
            pid: std::process::id(),
        })
    }
    #[test]
    fn repeated_authenticated_stop_preserves_one_budget_and_cannot_revive_it() {
        let binding = binding();
        let (mut parent, helper) = UnixStream::pair().unwrap();
        let mut control = ManagedControl {
            binding: binding.clone(),
            controller: controller_test_fixture(),
            finalization: None,
            stream: helper,
            request: None,
            authenticated: false,
        };
        assert!(control.finalization_budget().is_none());
        let request = stop(&binding);
        send_control_packet(&mut parent, &ParentCommand::Stop { request: request.clone() }, None).unwrap();
        assert!(control.poll_stop().unwrap());
        let budget = control.finalization_budget().unwrap();
        let original = budget.constraint();
        send_control_packet(&mut parent, &ParentCommand::Stop { request: request.clone() }, None).unwrap();
        assert!(control.poll_stop().unwrap());
        assert_eq!(control.finalization_budget().unwrap().constraint(), original);
        let mut changed = request.clone();
        changed.request_id = Uuid::new_v4().to_string();
        send_control_packet(&mut parent, &ParentCommand::Stop { request: changed }, None).unwrap();
        assert!(control.poll_stop().is_err());
        assert_eq!(control.finalization_budget().unwrap().constraint(), original);
        budget.cancel();
        send_control_packet(&mut parent, &ParentCommand::Stop { request }, None).unwrap();
        assert!(control.poll_stop().unwrap());
        assert!(control.finalization_budget().unwrap().remaining().is_err());
    }
    #[test]
    fn controller_witness_clones_retain_the_same_cloexec_kernel_identity() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<ControllerProcessWitness>();
        let controller = controller_test_fixture();
        let retained = Arc::clone(&controller);
        assert!(Arc::ptr_eq(&controller, &retained));
        let flags = unsafe { libc::fcntl(controller.lifetime.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        let (stream, _other) = UnixStream::pair().unwrap();
        controller.verify_peer(&stream).unwrap();
        drop(controller);
        retained.check_current().unwrap();
        retained.verify_peer(&stream).unwrap();
    }
    #[test]
    fn controller_witness_rejects_wrong_process_and_stays_fenced_after_exit() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        // This private unit fixture exercises the kernel lifetime after exit;
        // the integration test obtains its witness through the real handoff.
        let controller = ControllerProcessWitness {
            lifetime: pidfd(child.id()).unwrap(),
            pid: child.id(),
        };
        controller.check_current().unwrap();
        let (wrong_peer, _other) = UnixStream::pair().unwrap();
        assert!(controller.verify_peer(&wrong_peer).is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(controller.check_current().is_err());
        assert!(controller.verify_peer(&wrong_peer).is_err());
        // A different current process/socket cannot revive the original held
        // pidfd. No lookup or reopening by stored PID occurs during a check.
        let current = controller_test_fixture();
        current.verify_peer(&wrong_peer).unwrap();
        assert!(controller.check_current().is_err());
    }
    fn binding() -> ManagedExecutionBinding {
        ManagedExecutionBinding {
            format_version: 2,
            request_id: Uuid::new_v4().to_string(),
            profile: None,
            run_id: "run".into(),
            run_epoch: 3,
            execution_id: Uuid::new_v4().to_string(),
            operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(),
            owner: crate::protocol::OwnerEpoch {
                owner: "owner".into(),
                epoch: 7,
            },
            execution_profile: "claude-sdk".into(),
            frontend_start: ManagedFrontendStart::Immediate,
            server_command: None,
            guest_socket: "/tmp/cx.sock".into(),
        }
    }
    fn stop(binding: &ManagedExecutionBinding) -> StopAndCheckpoint {
        StopAndCheckpoint {
            request_id: Uuid::new_v4().to_string(),
            execution_id: binding.execution_id.clone(),
            operation_id: binding.operation_id.clone(),
            view_id: binding.view_id.clone(),
            run_id: binding.run_id.clone(),
            run_epoch: binding.run_epoch,
            owner: binding.owner.clone(),
        }
    }
    #[test]
    fn managed_stop_checks_every_reserved_fence_and_rejects_controller_disconnect() {
        let binding = binding();
        let (mut parent, helper) = UnixStream::pair().unwrap();
        let mut control = ManagedControl {
            binding: binding.clone(),
            controller: controller_test_fixture(),
            finalization: None,
            stream: helper,
            request: None,
            authenticated: false,
        };
        let request = stop(&binding);
        send_control_packet(
            &mut parent,
            &ParentCommand::Stop {
                request: request.clone(),
            },
            None,
        )
        .unwrap();
        assert!(control.poll_stop().unwrap());
        send_control_packet(
            &mut parent,
            &ParentCommand::Stop {
                request: request.clone(),
            },
            None,
        )
        .unwrap();
        assert!(
            control.poll_stop().unwrap(),
            "identical request reconciles the original shutdown"
        );
        let mut stale = request.clone();
        stale.run_epoch += 1;
        send_control_packet(&mut parent, &ParentCommand::Stop { request: stale }, None).unwrap();
        assert!(control.poll_stop().is_err());
        let mut stale = request.clone();
        stale.owner.epoch += 1;
        assert!(stale.validate(&binding).is_err());
        stale = request.clone();
        stale.execution_id = Uuid::new_v4().to_string();
        assert!(stale.validate(&binding).is_err());
        stale = request;
        stale.view_id = Uuid::new_v4().to_string();
        assert!(stale.validate(&binding).is_err());
        drop(parent);
        // Other tests fork real children concurrently. A fork inherits this
        // CLOEXEC descriptor until exec, so HUP is eventual rather than an
        // immediate single-poll observation on every supported runtime.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if control.poll_stop().is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "controller EOF did not become observable within its bound"
            );
            std::thread::yield_now();
        }
    }
    #[test]
    fn readiness_transfers_a_real_pidfd_bound_to_the_owned_attempt() {
        let binding = binding();
        let (mut parent, helper) = UnixStream::pair().unwrap();
        let mut control = ManagedControl {
            binding: binding.clone(),
            controller: controller_test_fixture(),
            finalization: None,
            stream: helper,
            request: None,
            authenticated: false,
        };
        let actual = pidfd(std::process::id()).unwrap();
        control.backend_ready(&actual).unwrap();
        let (event, descriptor): (ControlEvent, _) = receive_control_packet(&mut parent).unwrap();
        assert!(
            matches!(event, ControlEvent::BackendReady { execution_id, .. } if execution_id == binding.execution_id)
        );
        let witness = BackendProcessWitness::received(descriptor.unwrap(), &binding).unwrap();
        assert_eq!(witness.pid(), std::process::id());
        assert!(witness.matches_attempt(&binding.run_id, &binding.execution_id, "claude-sdk"));
        assert!(!witness.matches_attempt("another-run", &binding.execution_id, "claude-sdk"));
        witness.verify_peer(&parent).unwrap();
    }
    #[test]
    fn a_guest_socket_or_saved_pid_cannot_replace_the_owned_backend_process() {
        let binding = binding();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let actual = pidfd(child.id()).unwrap();
        let witness = BackendProcessWitness::received(actual, &binding).unwrap();
        let (stream, _) = UnixStream::pair().unwrap();
        assert!(
            witness.verify_peer(&stream).is_err(),
            "same-UID listener with another PID is not backend readiness"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            witness.check_current().is_err(),
            "pidfd lifetime survives PID reuse without restoring authority"
        );
        let ordinary = std::fs::File::open("/dev/null").unwrap();
        assert!(BackendProcessWitness::received(ordinary.into(), &binding).is_err());
    }
    #[test]
    fn checkpoint_acknowledgment_requires_dead_backend_and_exact_sealed_record_identity() {
        for case in ["alive", "digest", "helper", "phase", "valid"] {
            let binding = binding();
            let request = stop(&binding);
            let digest = "a".repeat(64);
            let (parent, mut helper) = UnixStream::pair().unwrap();
            helper
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            helper
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let lifetime = pidfd(std::process::id()).unwrap();
            let (commands, receive) = mpsc::sync_channel(1);
            commands
                .send(ParentCommand::Stop {
                    request: request.clone(),
                })
                .unwrap();
            let (events, receive_events) = mpsc::sync_channel(4);
            let worker_binding = binding.clone();
            let worker_digest = digest.clone();
            let worker = std::thread::spawn(move || {
                managed_client_loop(
                    parent,
                    &worker_binding,
                    &worker_digest,
                    std::process::id(),
                    &lifetime,
                    receive,
                    &events,
                    &std::sync::atomic::AtomicBool::new(false),
                )
            });
            let (received, descriptor): (ParentCommand, _) =
                receive_control_packet(&mut helper).unwrap();
            assert!(
                matches!(received, ParentCommand::Stop { request:received } if received == request)
            );
            assert!(descriptor.is_none());
            let mut child = std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap();
            let backend = pidfd(child.id()).unwrap();
            send_control_packet(
                &mut helper,
                &ControlEvent::BackendReady {
                    protocol_version: 2,
                    frontend_start: binding.frontend_start,
                    execution_id: binding.execution_id.clone(),
                    operation_id: binding.operation_id.clone(),
                    view_id: binding.view_id.clone(),
                },
                Some(&backend),
            )
            .unwrap();
            assert!(matches!(
                receive_events.recv_timeout(Duration::from_secs(2)).unwrap(),
                ManagedExecutionEvent::BackendReady { .. }
            ));
            if case != "alive" {
                child.kill().unwrap();
                child.wait().unwrap();
            }
            let mut record = ExecutionRecordV1 {
                format_version: 1,
                execution_id: binding.execution_id.clone(),
                operation_id: binding.operation_id.clone(),
                view_id: binding.view_id.clone(),
                owner: binding.owner.clone(),
                request_digest: digest,
                phase: ExecutionPhase::Checkpointed,
                supervisor_pid: std::process::id(),
                agent_exit: Some(143),
                checkpoint: Some(VfsCheckpoint {
                    format_version: 1,
                    directory: crate::protocol::ObjectRef::from_bytes(
                        ObjectKind::Directory,
                        b"fixture",
                    ),
                    hardlinks: crate::protocol::ObjectRef::from_bytes(ObjectKind::Blob, b"fixture"),
                }),
            };
            match case {
                "digest" => record.request_digest = "b".repeat(64),
                "helper" => record.supervisor_pid += 1,
                "phase" => record.phase = ExecutionPhase::Orphaned,
                _ => {}
            }
            let record = ExecutionRecord::V1(record);
            send_control_packet(
                &mut helper,
                &ControlEvent::Shutdown {
                    receipt: ManagedShutdownReceipt {
                        request,
                        record,
                        kernel_drained: true,
                    },
                },
                None,
            )
            .unwrap();
            let result = worker.join().unwrap();
            if case == "alive" {
                child.kill().unwrap();
                child.wait().unwrap();
            }
            if case == "valid" {
                assert!(result.is_ok());
                assert!(matches!(
                    receive_events.try_recv().unwrap(),
                    ManagedExecutionEvent::Shutdown(_)
                ));
            } else {
                assert!(
                    result.is_err(),
                    "{case} must withhold checkpoint acknowledgment"
                );
                assert!(receive_events.try_recv().is_err());
            }
        }
    }
    #[test]
    fn authenticated_frontend_requires_new_protocol_and_exact_parent_launch_nonce() {
        let mut binding = binding();
        binding.execution_profile = "codex-app-server".into();
        binding.frontend_start = ManagedFrontendStart::AfterHostApiKeyV1;
        binding.server_command = Some(vec!["/usr/bin/codex".into()]);
        let mut old = serde_json::to_value(&binding).unwrap();
        old.as_object_mut().unwrap().remove("frontend_start");
        assert!(
            serde_json::from_value::<ManagedExecutionBinding>(old).is_err(),
            "old unguarded managed protocol cannot authorize bootstrap"
        );
        let old_ready = serde_json::json!({"event":"backend_ready","execution_id":binding.execution_id,"operation_id":binding.operation_id,"view_id":binding.view_id});
        assert!(
            serde_json::from_value::<ControlEvent>(old_ready).is_err(),
            "readiness must prove the frontend gate before any key leaves host"
        );
        for stale in [true, false] {
            let (mut parent, helper) = UnixStream::pair().unwrap();
            let mut control = ManagedControl {
                binding: binding.clone(),
                controller: controller_test_fixture(),
            finalization: None,
                stream: helper,
                request: None,
                authenticated: false,
            };
            let mut request = stop(&binding);
            if !stale {
                request.request_id = binding.request_id.clone();
            }
            send_control_packet(
                &mut parent,
                &ParentCommand::BackendAuthenticated { request },
                None,
            )
            .unwrap();
            if stale {
                assert!(control.poll_stop().is_err());
                assert!(!control.authenticated);
            } else {
                assert!(!control.poll_stop().unwrap());
                assert!(control.authenticated);
                let request = stop(&binding);
                send_control_packet(&mut parent, &ParentCommand::Stop { request }, None).unwrap();
                assert!(
                    control.poll_stop().unwrap(),
                    "authenticated frontend never absorbs a later parent stop"
                );
            }
        }
    }
    #[test]
    fn control_wire_preserves_record2_integer_tokens_and_rejects_duplicate_fields() {
        let binding = binding();
        let request = stop(&binding);
        let run_epoch = (1_u64 << 53) + 1;
        let record = ExecutionRecord::V2(super::super::ExecutionRecordV2 {
            format_version: 2,
            identity: super::super::ExecutionIdentityV2 {
                project_id: "project".into(),
                request_id: binding.request_id.clone(),
                run_id: binding.run_id.clone(),
                run_epoch,
                execution_id: binding.execution_id.clone(),
                operation_id: binding.operation_id.clone(),
                view_id: binding.view_id.clone(),
                owner: binding.owner.clone(),
                request_digest: "a".repeat(64),
                supervisor_pid: std::process::id(),
            },
            state: super::super::ExecutionStateV2::Preparing,
        });
        let event = ControlEvent::Shutdown {
            receipt: ManagedShutdownReceipt {
                request,
                record,
                kernel_drained: true,
            },
        };
        let bytes = String::from_utf8(crate::protocol::canonical_json(&event).unwrap()).unwrap();
        let decoded: ControlEvent = serde_json::from_str(&bytes).unwrap();
        let ControlEvent::Shutdown { receipt } = decoded else {
            panic!("wrong event")
        };
        let ExecutionRecord::V2(record) = receipt.record else {
            panic!("wrong record codec")
        };
        assert_eq!(record.identity.run_epoch, run_epoch);
        for malformed in [
            bytes.replacen(
                "\"event\":\"shutdown\"",
                "\"event\":\"shutdown\",\"event\":\"shutdown\"",
                1,
            ),
            bytes.replacen(
                "\"format_version\":2",
                "\"format_version\":2,\"format_version\":2",
                1,
            ),
            bytes.replacen(
                "\"phase\":\"preparing\"",
                "\"phase\":\"preparing\",\"extra\":true",
                1,
            ),
            bytes.replacen(
                "\"event\":\"shutdown\"",
                "\"event\":\"shutdown\",\"extra\":true",
                1,
            ),
            bytes.replacen("\"event\":\"shutdown\"", "\"event\":\"future\"", 1),
        ] {
            assert!(serde_json::from_str::<ControlEvent>(&malformed).is_err());
        }
    }
    #[test]
    fn v4_descriptor_frame_preserves_all_fds_and_legacy_receiver_refuses_it() {
        use std::os::fd::AsRawFd;
        let originals: Vec<OwnedFd> = (0..3)
            .map(|_| std::fs::File::open("/dev/null").unwrap().into())
            .collect();
        let raw: Vec<_> = originals.iter().map(AsRawFd::as_raw_fd).collect();
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        send_control_packet_fds(&mut sender, &u64::MAX, &raw).unwrap();
        let (value, received): (u64, _) = receive_control_packet_fds(&mut receiver).unwrap();
        assert_eq!(value, u64::MAX);
        assert_eq!(received.len(), 3);
        for fd in &received {
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            assert!(
                super::super::network::linux::NamespaceFd::inspect(
                    fd.try_clone().unwrap(),
                    super::super::network::linux::NamespaceKind::User
                )
                .is_err(),
                "a descriptor transport does not authenticate a namespace"
            );
        }
        send_control_packet_fds(&mut sender, &true, &raw).unwrap();
        assert!(
            receive_control_packet::<bool>(&mut receiver).is_err(),
            "legacy control accepts at most one FD"
        );
        for fd in &originals {
            assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } >= 0);
        }
    }
}
