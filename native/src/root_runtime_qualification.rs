//! Disposable runtime-role qualification only. Actual original Root/Native
//! owners are mandatory; JSON, UID0 and historical ACKs issue no authority.
use super::*;
use rudder_engine::protocol::{ObjectKind, ObjectRef};
use std::os::unix::fs::PermissionsExt;

pub(in crate::root_storage_bootstrap) const LIMIT: usize = 16 * 1024;
const SCOPE: &str = "disposable-runtime-floor-and-original-client-only";
const PATHS: [&str; 4] = ["package/dist/native/rudder-native", "package/dist/native/rudder-jj",
    "usr/local/bin/rudder-root-storage", "usr/local/bin/rudder-machine-root"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeQualificationScope {
    pub format_version: u32, pub scope: String,
    pub qualification_id: String, pub qualification_nonce: String,
    pub image: String, pub image_receipt_sha256: String, pub source_manifest_sha256: String,
    pub inventory: ObjectRef, pub manifest: ObjectRef, pub installer_input: ObjectRef,
    pub native_sha256: String, pub buddy_sha256: String, pub service_sha256: String, pub launcher_sha256: String,
}
fn hash(value: &str) -> Result<()> {
    ensure!(value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "qualification SHA invalid"); Ok(())
}
fn uuid4(value: &str) -> Result<()> {
    let id = uuid::Uuid::parse_str(value)?;
    ensure!(id.get_version_num() == 4 && id.to_string() == value, "qualification original UUID invalid"); Ok(())
}
fn decimal(value: &str) -> Result<u128> {
    ensure!(!value.is_empty() && value.len() <= 20 && !value.starts_with('0') && value.bytes().all(|b| b.is_ascii_digit()), "qualification decimal invalid"); Ok(value.parse()?)
}
impl RuntimeQualificationScope {
    pub(in crate::root_storage_bootstrap) fn validate_config(&self, config: &HostIngressConfig) -> Result<()> {
        ensure!(self.format_version == 1 && self.scope == SCOPE, "qualification scope differs");
        uuid4(&self.qualification_id)?; uuid4(&self.qualification_nonce)?;
        for h in [&self.image_receipt_sha256, &self.source_manifest_sha256, &self.native_sha256,
            &self.buddy_sha256, &self.service_sha256, &self.launcher_sha256] { hash(h)?; }
        let digest = self.image.strip_prefix("registry.fly.io/rudder-workers-exla@sha256:").context("qualification image not pinned")?; hash(digest)?;
        for (reference, kind, bound) in [(&self.inventory, ObjectKind::Blob, 16 * 1024 * 1024),
            (&self.manifest, ObjectKind::Blob, 32 * 1024 * 1024), (&self.installer_input, ObjectKind::Directory, 64 * 1024)] {
            reference.validate()?; ensure!(reference.kind == kind && reference.size_bytes > 0 && reference.size_bytes <= bound, "qualification content reference bound");
        }
        let policy = config.image_preseed.as_ref().context("qualification image policy absent")?;
        ensure!(policy.image == self.image && policy.image_receipt_sha256 == self.image_receipt_sha256
            && policy.full_manifest_sha256 == self.manifest.digest
            && policy.runtime_mount.as_ref().is_some_and(|p| p.inventory == self.inventory), "qualification differs from actual protected image policy"); Ok(())
    }
    fn selected(&self, entries: &[wire::RuntimeExecutableData]) -> Result<()> {
        ensure!(entries.len() == 4, "qualification requires four selected executables");
        for ((entry, path), expected) in entries.iter().zip(PATHS).zip([
            &self.native_sha256, &self.buddy_sha256, &self.service_sha256, &self.launcher_sha256]) {
            hash(&entry.sha256)?; ensure!(entry.installer_path == path && &entry.sha256 == expected
                && entry.size_bytes > 0 && entry.size_bytes <= 512 * 1024 * 1024, "qualification selected original image differs");
        } Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::root_storage_bootstrap) struct NativeExportData {
    pub version: u32, pub kind: String, pub scope: RuntimeQualificationScope, pub observation: serde_json::Value,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::root_storage_bootstrap) struct Envelope<T> { pub body: T, pub mac: String }
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::root_storage_bootstrap) struct RequestBody {
    pub version: u32, pub kind: String, pub qualification_id: String, pub qualification_nonce: String,
    pub registration_binding: engine_registration::RegistrationBinding,
    pub root_nonce: String, pub registration_proof_sha256: String,
    pub scope: RuntimeQualificationScope, pub observation: serde_json::Value,
    pub selected_executables: Vec<wire::RuntimeExecutableData>, pub remaining_milliseconds: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::root_storage_bootstrap) struct PublishedData {
    pub scope: String, pub state: String, pub role_binding_sha256: String, pub image: String,
    pub inventory: ObjectRef, pub manifest: ObjectRef, pub installer_input: ObjectRef,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::root_storage_bootstrap) struct AckBody {
    pub version: u32, pub kind: String, pub qualification_id: String, pub qualification_nonce: String,
    pub request_sha256: String, pub registration_proof_sha256: String,
    pub original_enable_constraint: FinalizationConstraint, pub result: Option<PublishedData>, pub error: Option<String>,
}

// Original writable and readonly memfd handles are staged before every write,
// seal and open. Failures retain them; no IO/unpin/retry happens in Drop.
pub(in crate::root_storage_bootstrap) struct WitnessFile { writer: Option<File>, read: Option<OwnedFd>, attempted: bool }
impl WitnessFile {
    fn staged() -> Self { Self { writer: None, read: None, attempted: false } }
    fn build(&mut self, raw: &[u8]) -> Result<()> {
        ensure!(!self.attempted && !raw.is_empty() && raw.len() <= LIMIT, "original qualification witness cannot retry/exceeds bound"); self.attempted = true;
        let fd = unsafe { libc::memfd_create(c"rudder-runtime-qualification".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        ensure!(fd >= 0, "qualification witness creation failed"); self.writer = Some(File::from(unsafe { OwnedFd::from_raw_fd(fd) }));
        self.writer.as_mut().unwrap().write_all(raw)?;
        self.writer.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;
        ensure!(unsafe { libc::fcntl(self.writer.as_ref().unwrap().as_raw_fd(), libc::F_ADD_SEALS, super::super::SEALS) } == 0, "qualification witness sealing failed");
        self.read = Some(OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", self.writer.as_ref().unwrap().as_raw_fd()))?.into());
        ensure!(storage::id(&File::from(self.read.as_ref().unwrap().try_clone()?))? == storage::id(self.writer.as_ref().unwrap())?, "qualification witness identity changed"); Ok(())
    }
    fn fd(&self) -> Result<&OwnedFd> { self.read.as_ref().context("original qualification witness incomplete") }
}
fn read_native_witness(fd: &OwnedFd) -> Result<Vec<u8>> {
    let file = File::from(fd.try_clone()?); let before = file.metadata()?;
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    ensure!(before.is_file() && before.uid() == 1500 && before.gid() == 1500 && before.nlink() == 0
        && before.mode() & 0o7777 == 0o444 && before.len() > 0 && before.len() <= LIMIT as u64
        && flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY
        && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) } & super::super::SEALS == super::super::SEALS, "qualification export is not actual sealed Native witness");
    let mut raw = vec![0; before.len() as usize];
    ensure!(file.read_at(&mut raw, 0)? == raw.len() && stamp(&file)? == (before.dev(), before.ino(), before.len(), before.mode(), before.ctime(), before.ctime_nsec()), "original qualification witness changed"); Ok(raw)
}

pub(in crate::root_storage_bootstrap) struct NativeQualification {
    client: Arc<RuntimeImageClient>, data: Option<NativeExportData>, witness: WitnessFile,
    frame: Option<Vec<u8>>, sent: bool, unknown: bool,
}
impl NativeQualification {
    fn staged(client: Arc<RuntimeImageClient>) -> Self { Self { client, data: None, witness: WitnessFile::staged(), frame: None, sent: false, unknown: false } }
    fn poll(&mut self, owner: &RuntimeReadyObservation, scope: &RuntimeQualificationScope, gate: &mut super::super::Gate) -> Result<()> {
        ensure!(!self.unknown, "qualification export unknown; retain original owner"); if self.sent { return Ok(()); }
        ensure!(owner.matches_original_client(&self.client), "qualification export replaced original Native client");
        if self.data.is_none() {
            let observation = owner.checked_original_qualification_export_off_app()?;
            scope.selected(&observation.selected_executables)?;
            ensure!(observation.inventory == scope.inventory && observation.manifest == scope.manifest
                && observation.installer_input == scope.installer_input, "actual Native observation differs from qualification roots");
            self.data = Some(NativeExportData { version: 1, kind: "original-runtime-qualification-export-v1".into(), scope: scope.clone(), observation: serde_json::to_value(observation)? });
            self.witness.build(&canonical_json(self.data.as_ref().unwrap())?)?;
        }
        owner.assert_original_qualification_window_off_app()?; gate.peer.live()?;
        if self.frame.is_none() { self.frame = Some(gate.frame(super::super::Message::RuntimeImageQualificationExport)?); }
        match gate.endpoint.send(self.frame.as_ref().unwrap(), Some(self.witness.fd()?)) {
            Ok(false) => Ok(()), Ok(true) => { self.sent = true; owner.assert_original_qualification_window_off_app() },
            Err(error) => { self.unknown = true; Err(error) },
        }
    }
}

impl super::super::ManagedMachineBootstrap {
    /// OFF App; only the SAME original HostServing worker can export the
    /// already-issued opaque observation. No DATA/boolean owner constructor.
    pub fn export_runtime_qualification_retaining(&mut self, owner: &RuntimeReadyObservation) -> Result<()> {
        let registration = self.registered_engine.as_ref().context("qualification actual registration absent")?;
        let config = registration.runtime_config_owner();
        let Some(scope) = config.data().runtime_qualification.as_ref() else { return Ok(()); };
        let worker = match self.host_ack { super::super::HostServingAck::Sent(tid) => tid, _ => anyhow::bail!("qualification original worker ACK absent") };
        ensure!(unsafe { libc::syscall(libc::SYS_gettid) } as u32 == worker, "qualification export is not original HostServing worker");
        scope.validate_config(config.data())?; self.check_current_retaining()?;
        let client = self.runtime_client.as_ref().context("qualification actual client absent")?;
        ensure!(owner.matches_original_client(client), "qualification observation replaced actual client");
        if self.runtime_qualification.is_none() { self.runtime_qualification = Some(NativeQualification::staged(client.clone())); }
        self.runtime_qualification.as_mut().unwrap().poll(owner, scope, self.gate.as_mut().unwrap())
    }
}

const OBSERVATION_FIELDS: [&str; 25] = ["format_version", "kind", "scope", "request_nonce", "bootstrap_nonce", "engine_instance_id", "epoch", "mount_id_data", "original_enable_constraint",
    "root_ack_observed_boottime_nanoseconds", "native_received_boottime_nanoseconds", "native_issued_boottime_nanoseconds", "client_current_checked_boottime_nanoseconds",
    "registration_proof_sha256", "config_sha256", "boot_id", "service_pid_data", "source_authority", "helper_enrolled", "scheduler_ready", "normal_retirement_authority",
    "inventory", "manifest", "installer_input", "selected_executables"];
impl RootRuntimeParent {
    pub(in crate::root_storage_bootstrap) fn selected_qualification_executables_data(&self) -> Result<Vec<wire::RuntimeExecutableData>> {
        self.original()?.remaining()?;
        ensure!(self.ready && !self.failed, "qualification original runtime ACK unavailable");
        Ok(self.accepted_ack.as_ref().context("qualification actual ACK absent")?.selected_executables.clone())
    }
    pub(in crate::root_storage_bootstrap) fn check_original_qualification_export(&self, export: &NativeExportData,
        registration: &engine_registration::CompletedRuntimeRegistration, config: &HostIngressConfig) -> Result<()> {
        self.original()?.remaining()?;
        ensure!(self.ready && !self.failed && self.endpoint_offered && self.ready_offered && self.pending.is_some(), "qualification lacks original Root ready/endpoint custody");
        ensure!(export.version == 1 && export.kind == "original-runtime-qualification-export-v1"
            && config.runtime_qualification.as_ref() == Some(&export.scope), "qualification export scope differs from protected Root config");
        export.scope.validate_config(config)?;
        let evidence = self.ready_evidence.as_ref().context("qualification actual Root ready evidence absent")?;
        let ack = self.accepted_ack.as_ref().context("qualification actual original ACK absent")?;
        let request = self.request.as_ref().context("qualification original request absent")?;
        ensure!(canonical_json(ack)? == canonical_json(&evidence.ack)? && request.request_nonce == ack.request_nonce
            && request.bootstrap_nonce == registration.root_nonce && ack.engine_instance_id == registration.ack.engine_instance_id
            && ack.epoch == registration.ack.binding.epoch, "qualification original Root ACK/request/registration differs");
        export.scope.selected(&ack.selected_executables)?;
        ensure!(ack.inventory == export.scope.inventory && ack.manifest == export.scope.manifest
            && ack.installer_input == export.scope.installer_input, "qualification actual Root mount roots differ");
        let d = &export.observation; let object = d.as_object().context("qualification observation not object")?;
        ensure!(object.len() == OBSERVATION_FIELDS.len() && OBSERVATION_FIELDS.iter().all(|key| object.contains_key(*key)), "qualification observation exact schema differs");
        ensure!(d["format_version"] == 1 && d["kind"] == "runtime-image-ready-observation-data-v1"
            && d["scope"] == "original-enable-ack-and-current-original-client-only"
            && d["request_nonce"] == ack.request_nonce && d["bootstrap_nonce"] == ack.bootstrap_nonce
            && d["engine_instance_id"] == ack.engine_instance_id && d["epoch"] == ack.epoch
            && d["mount_id_data"] == ack.mount_id.to_string() && d["original_enable_constraint"] == serde_json::to_value(self.original()?.constraint())?
            && d["root_ack_observed_boottime_nanoseconds"] == evidence.root_ack_observed_boottime_nanoseconds
            && d["registration_proof_sha256"] == registration.ack.proof_sha256 && d["config_sha256"] == registration.ack.binding.config_sha256
            && d["boot_id"] == registration.ack.boot_id && d["service_pid_data"] == evidence.service_pid
            && d["inventory"] == serde_json::to_value(&ack.inventory)? && d["manifest"] == serde_json::to_value(&ack.manifest)?
            && d["installer_input"] == serde_json::to_value(&ack.installer_input)?
            && d["selected_executables"] == serde_json::to_value(&ack.selected_executables)?, "qualification observation differs from actual original Root receipt");
        for key in ["source_authority", "helper_enrolled", "scheduler_ready", "normal_retirement_authority"] { ensure!(d[key] == false, "qualification observation claims unrelated authority"); }
        let root = &evidence.root_ack_observed_boottime_nanoseconds;
        let received = d["native_received_boottime_nanoseconds"].as_str().context("qualification original Native receipt absent")?;
        let issued = d["native_issued_boottime_nanoseconds"].as_str().context("qualification original Native issue absent")?;
        let checked = d["client_current_checked_boottime_nanoseconds"].as_str().context("qualification original Native check absent")?;
        let now = super::super::boottime_stamp()?;
        ready::original_receipt_window(&request.constraint, root, &now)?;
        ensure!(decimal(root)? <= decimal(received)? && decimal(received)? <= decimal(issued)?
            && decimal(issued)? <= decimal(checked)? && decimal(checked)? <= decimal(&now)?, "qualification actual observation clock order differs");
        self.original()?.remaining()?; Ok(())
    }
}

/// Actual Root owns this carrier alongside the original registration/runtime/
/// process owners. No raw constructor issues a published receipt or role grant.
pub(in crate::root_storage_bootstrap) struct RootQualification {
    original_fd: OwnedFd, export: Option<NativeExportData>, original: Option<FinalizationBudget>,
    request: Option<Envelope<RequestBody>>, wire: Option<Vec<u8>>, sent: bool,
    pending: Option<storage::Packet>, ack: Option<Envelope<AckBody>>, published: bool, closed: bool,
    failure: Option<anyhow::Error>, duplicates: Vec<OwnedFd>,
    observation: super::super::StartupObservationOwner,compiler:Option<super::compiler_qualification::RootCompilerQualification>,
}
impl RootQualification {
    pub(in crate::root_storage_bootstrap) fn staged(fd: OwnedFd) -> Self { Self { original_fd: fd, export: None, original: None,
        request: None, wire: None, sent: false, pending: None, ack: None, published: false, closed: false, failure: None, duplicates: Vec::new(),
        observation: super::super::StartupObservationOwner::new(),compiler:None } }
    pub(in crate::root_storage_bootstrap) fn duplicate(&mut self, fd: OwnedFd) -> Result<()> { self.duplicates.push(fd); self.closed = true; anyhow::bail!("duplicate original qualification export retained") }
    pub(in crate::root_storage_bootstrap) fn closed(&self) -> bool { self.closed || self.published }
    pub(in crate::root_storage_bootstrap) fn poll_retaining(&mut self, runtime: &RootRuntimeParent, registration: &mut engine_registration::RootRegistration, host: &mut RootHostControl) -> Result<()> {
        ensure!(!self.closed, "original qualification closed/unknown; no replay"); if self.published { return Ok(()); }
        let result = self.poll_inner(runtime, registration, host);
        if let Err(error) = result { self.closed = true; self.failure = Some(error); anyhow::bail!("original runtime qualification unknown; retain every owner"); } Ok(())
    }
    fn poll_inner(&mut self, runtime: &RootRuntimeParent, registration: &mut engine_registration::RootRegistration, host: &mut RootHostControl) -> Result<()> {
        if self.original.is_none() { self.original = Some(runtime.original()?.clone()); }
        self.original.as_ref().unwrap().remaining()?;
        registration.qualification_original_registration_current(runtime)?;
        if !host.runtime_qualification_transport_current()? {return Ok(());}
        if self.export.is_none() { let raw = read_native_witness(&self.original_fd)?;
            self.export = Some(serde_json::from_slice(&raw)?); ensure!(canonical_json(self.export.as_ref().unwrap())? == raw, "qualification Native export not canonical"); }
        if let Some(scope)=host.registration_config().compiler_qualification.as_ref(){
            if self.compiler.is_none(){self.compiler=Some(super::compiler_qualification::RootCompilerQualification::staged(scope.clone()));}
            if !self.compiler.as_mut().unwrap().poll_retaining(runtime,registration,host,self.export.as_ref().unwrap())?{return Ok(())}
            self.published=true;
            observe_qualification_once(&mut self.observation,libc::STDERR_FILENO,||Ok(b"MACHINE_COMPILER_SDK_QUALIFICATION_PUBLISHED\n".to_vec()));return Ok(())
        }
        if self.request.is_none() {
            self.request = Some(registration.sign_original_runtime_qualification(runtime, self.export.as_ref().unwrap())?);
            self.wire = Some(canonical_json(&serde_json::json!({"kind":"runtime_image_qualification_ready_v1",
                "rpc_id":self.request.as_ref().unwrap().body.observation["request_nonce"], "packet":self.request}))?);
            ensure!(self.wire.as_ref().unwrap().len() <= LIMIT, "qualification relay bound");
        }
        if !self.sent {
            self.original.as_ref().unwrap().remaining()?;
            // false is known EAGAIN, with the SAME signed body/nonce/cutoff.
            // Any uncertain send closes this carrier; it is never replayed.
            if !host.registration_send(self.wire.as_ref().unwrap())? { return Ok(()); }
            self.sent = true; self.original.as_ref().unwrap().remaining()?;
        }
        if self.pending.is_none() { host.runtime_qualification_receive_retaining(&mut self.pending)?; }
        let Some(packet) = self.pending.as_ref() else { return Ok(()); };
        let raw:serde_json::Value=serde_json::from_slice(&packet.bytes)?;
        let body=raw.get("packet").and_then(|p|p.get("body")).and_then(serde_json::Value::as_object)
            .context("qualification original ACK body absent")?;
        let fields=["version","kind","qualification_id","qualification_nonce","request_sha256",
            "registration_proof_sha256","original_enable_constraint","result","error"];
        ensure!(body.len()==fields.len()&&fields.iter().all(|key|body.contains_key(*key)),
            "qualification ACK body requires its exact fields, including explicit result/error");
        #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct Relay { kind: String, rpc_id: String, packet: Envelope<AckBody> }
        let relay: Relay = serde_json::from_slice(&packet.bytes)?;
        self.ack = Some(relay.packet);
        ensure!(relay.kind == "runtime_image_qualification_ack_v1" && self.request.as_ref().unwrap().body.observation["request_nonce"] == relay.rpc_id, "qualification ACK correlation differs");
        registration.verify_original_runtime_qualification_ack(runtime, self.request.as_ref().unwrap(), self.ack.as_ref().unwrap())?;
        self.original.as_ref().unwrap().remaining()?; self.published = true;
        // Secondary DATA only, after actual publication. This helper receives
        // only the retained observation slot, never publication/health state.
        // No observer error or panic can escape into poll_retaining/quarantine.
        let ack = &self.ack;
        observe_qualification_once(&mut self.observation, libc::STDERR_FILENO, || {
            let ack = &ack.as_ref().context("qualification observation ACK absent")?.body;
            let result = ack.result.as_ref().context("qualification observation result absent")?;
            Ok(format!("MACHINE_RUNTIME_QUALIFICATION_PUBLISHED {} {} {}\n",
                ack.qualification_id, ack.request_sha256, result.role_binding_sha256).into_bytes())
        });
        Ok(())
    }
}

// Single best-effort attempt using the same private bounded/SIGPIPE-safe
// writer as machine startup. Retain actual bytes, duplicated CLOEXEC output,
// delivery and any returned error/panic; never retry or create another budget.
// A raw descriptor here is only an observation destination, not an authority.
fn observe_qualification_once(
    observation: &mut super::super::StartupObservationOwner,
    output: std::os::fd::RawFd,
    bytes: impl FnOnce() -> Result<Vec<u8>>,
) {
    use super::super::{StartupObservationDelivery, OBSERVATION_LIMIT, try_observation_write};
    if observation.attempted { return; }
    observation.attempted = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<StartupObservationDelivery> {
        observation.bytes = Some(bytes()?);
        let bytes = observation.bytes.as_ref().context("qualification observation bytes absent")?;
        ensure!(!bytes.is_empty() && bytes.len() <= OBSERVATION_LIMIT, "qualification observation exceeds single-write bound");
        let raw = unsafe { libc::fcntl(output, libc::F_DUPFD_CLOEXEC, 20) };
        if raw < 0 { return Err(io::Error::last_os_error()).context("qualification observation original stderr unavailable"); }
        observation.output = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        observation.delivery = StartupObservationDelivery::Uncertain;
        try_observation_write(observation.output.as_ref().unwrap(), bytes)
    }));
    match result {
        Ok(Ok(delivery)) => observation.delivery = delivery,
        Ok(Err(error)) => {
            if observation.delivery != StartupObservationDelivery::Uncertain {
                observation.delivery = StartupObservationDelivery::Unavailable;
            }
            observation.error = Some(error);
        }
        Err(panic) => {
            observation.delivery = StartupObservationDelivery::Uncertain;
            observation.panic = Some(panic);
        }
    }
}

#[cfg(test)]
#[path="root_runtime_qualification_tests.rs"]
mod tests;
#[cfg(test)]
#[path="root_runtime_qualification_observation_tests.rs"]
mod observation_tests;
