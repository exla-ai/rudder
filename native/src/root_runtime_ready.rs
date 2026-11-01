//! Original enable ACK observation, not another runtime/source/task grant.
//! Constructors are private to the actual bootstrap/client receipt path.
use super::*;

pub(super) const READY_LIMIT: usize = 4096;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadyEvidenceData {
    pub(super) version: u32,
    pub(super) kind: String,
    pub(super) request: Enable,
    pub(super) ack: EnableAck,
    pub(super) service_pid: u32,
    pub(super) service_start_ticks: String,
    pub(super) registration_proof_sha256: String,
    pub(super) config_sha256: String,
    pub(super) boot_id: String,
    pub(super) root_ack_observed_boottime_nanoseconds: String,
}

fn decimal(value: &str) -> Result<u128> {
    ensure!(!value.is_empty() && value.len() <= 20 && value.as_bytes()[0] != b'0'
        && value.bytes().all(|b| b.is_ascii_digit()), "runtime ready original time DATA invalid");
    Ok(value.parse()?)
}
/// Validate the SAME absolute kernel cutoff at original receipt. No begin(),
/// budget adoption, later readback, lease or replacement deadline is created.
pub(super) fn original_receipt_window(constraint: &FinalizationConstraint, root: &str, native: &str) -> Result<()> {
    let data = serde_json::to_value(constraint)?;
    ensure!(data["format_version"] == 1 && data["maximum_milliseconds"] == 30000,
        "runtime ready original enable bound changed");
    let end = decimal(data["boottime_deadline_nanoseconds"].as_str().context("runtime ready original cutoff absent")?)?;
    let root = decimal(root)?; let native = decimal(native)?;
    ensure!(end >= 30_000_000_000 && root >= end - 30_000_000_000 && root <= native && native < end,
        "runtime ready original receipt was late/reordered"); Ok(())
}

/// Kept by the client, including the actual sealed original Root-channel FD.
/// Root separately retains the real service RuntimePacket and parsed EnableAck.
pub(super) struct ReadyOrigin {
    file: File,
    stamp: (u64,u64,u64,u32,i64,i64),
    data: ReadyEvidenceData,
    native_received_boottime_nanoseconds: String,
    observation_issued: AtomicBool,
}
impl ReadyOrigin {
    pub(super) fn capture_from_original_bootstrap(fd: &OwnedFd, registration: &RegisteredEngineOwner,
        service: &Process, bootstrap_nonce: &str) -> Result<Self> {
        // Caller is ONLY RuntimeImageClient's private constructor invoked by
        // the actual authenticated bootstrap's ordered endpoint receipt.
        let file = File::from(fd.try_clone()?); let before = stamp(&file)?;
        let bytes = super::super::read_sealed_bytes(fd, READY_LIMIT)?;
        let data: ReadyEvidenceData = serde_json::from_slice(&bytes)?;
        let received = super::super::boottime_stamp()?;
        original_receipt_window(&data.request.constraint, &data.root_ack_observed_boottime_nanoseconds, &received)?;
        let r = registration.data();
        ensure!(data.version == 1 && data.kind == "original-runtime-enable-ready-evidence-v1"
            && data.request.version == 1 && data.request.kind == "runtime_image_enable_v1"
            && data.request.bootstrap_nonce == bootstrap_nonce && data.request.bootstrap_nonce == r.root_nonce
            && data.ack.version == 1 && data.ack.kind == "runtime_image_ready_v1"
            && data.ack.bootstrap_nonce == data.request.bootstrap_nonce
            && data.ack.request_nonce == data.request.request_nonce
            && data.ack.engine_instance_id == r.engine_instance_id && data.ack.epoch == r.binding.epoch
            && data.ack.mount_id > 0 && data.service_pid == service.pid
            && decimal(&data.service_start_ticks).is_ok()
            && data.registration_proof_sha256 == r.proof_sha256 && data.config_sha256 == r.binding.config_sha256
            && data.boot_id == r.boot_id, "runtime ready changed original Root/service/registration binding");
        let nonce = uuid::Uuid::parse_str(&data.request.request_nonce)?;
        ensure!(nonce.get_version_num() == 4 && nonce.to_string() == data.request.request_nonce,
            "runtime ready request nonce is not original canonical UUID4");
        // The fields are mandatory original mount ACK DATA, never config-only
        // defaults. Registration binds the three process images; the scoped
        // qualification additionally binds the selected buddy executable.
        for (r,kind,bound) in [(&data.ack.inventory,rudder_engine::protocol::ObjectKind::Blob,16*1024*1024),
            (&data.ack.manifest,rudder_engine::protocol::ObjectKind::Blob,32*1024*1024),
            (&data.ack.installer_input,rudder_engine::protocol::ObjectKind::Directory,64*1024)] {
            r.validate()?; ensure!(r.kind==kind&&r.size_bytes>0&&r.size_bytes<=bound,"runtime ready actual ACK root profile differs");
        }
        let config=registration.runtime_config_owner();
        let policy=config.data().image_preseed.as_ref().context("runtime ready actual image policy absent")?;
        ensure!(policy.runtime_mount.as_ref().is_some_and(|p|p.inventory==data.ack.inventory)
            &&policy.full_manifest_sha256==data.ack.manifest.digest&&data.ack.selected_executables.len()==4,
            "runtime ready actual ACK roots/images differ");
        let paths=["package/dist/native/rudder-native","package/dist/native/rudder-jj","usr/local/bin/rudder-root-storage","usr/local/bin/rudder-machine-root"];
        for (entry,path) in data.ack.selected_executables.iter().zip(paths) {
            ensure!(entry.installer_path==path&&entry.sha256.len()==64&&entry.sha256.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))
                &&entry.size_bytes>0&&entry.size_bytes<=512*1024*1024,"runtime ready selected executable profile differs");
        }
        ensure!(data.ack.selected_executables[0].sha256==r.binding.native_sha256
            &&data.ack.selected_executables[2].sha256==r.binding.service_sha256
            &&data.ack.selected_executables[3].sha256==r.binding.launcher_sha256,"runtime ready selected original process images differ");
        registration.assert_current()?; service.live()?;
        ensure!(stamp(&file)? == before, "runtime ready original sealed witness changed");
        original_receipt_window(&data.request.constraint, &data.root_ack_observed_boottime_nanoseconds,
            &super::super::boottime_stamp()?)?;
        Ok(Self { file, stamp: before, data, native_received_boottime_nanoseconds: received, observation_issued: AtomicBool::new(false) })
    }
    pub(super) fn compiler_sdk_data(&self)->Result<Option<&super::compiler_sdk::CompilerSdkEnableData>> {self.current()?;Ok(self.data.ack.compiler_sdk.as_ref())}
    pub(super) fn original_receipt_current(&self) -> Result<()> {
        original_receipt_window(&self.data.request.constraint, &self.data.root_ack_observed_boottime_nanoseconds,
            &super::super::boottime_stamp()?)
    }
    fn current(&self) -> Result<()> {
        ensure!(stamp(&self.file)? == self.stamp, "runtime ready original witness changed"); Ok(())
    }
}

/// Serializable historical/current-client DATA only. It cannot construct the
/// opaque owner or a runtime mount/helper/source/scheduler/retirement grant.
#[derive(Clone, Debug, Serialize)]
pub struct RuntimeReadyObservationData {
    pub format_version: u32,
    pub kind: &'static str,
    pub scope: &'static str,
    pub request_nonce: String,
    pub bootstrap_nonce: String,
    pub engine_instance_id: String,
    pub epoch: String,
    pub mount_id_data: String,
    pub original_enable_constraint: FinalizationConstraint,
    pub root_ack_observed_boottime_nanoseconds: String,
    pub native_received_boottime_nanoseconds: String,
    pub native_issued_boottime_nanoseconds: String,
    pub client_current_checked_boottime_nanoseconds: String,
    pub registration_proof_sha256: String,
    pub config_sha256: String,
    pub boot_id: String,
    pub service_pid_data: u32,
    pub inventory: rudder_engine::protocol::ObjectRef,
    pub manifest: rudder_engine::protocol::ObjectRef,
    pub installer_input: rudder_engine::protocol::ObjectRef,
    pub selected_executables: Vec<wire::RuntimeExecutableData>,
    pub source_authority: bool,
    pub helper_enrolled: bool,
    pub scheduler_ready: bool,
    pub normal_retirement_authority: bool,
}

/// One non-Serde/non-Clone owner issued DURING original client adoption, never
/// by a later data getter. Retains the SAME client and its actual service,
/// protected config, registered engine/Root/worker and sealed evidence owners.
#[must_use = "retain the original ready/client/service/config/registration owners"]
pub struct RuntimeReadyObservation { client: Arc<RuntimeImageClient>, issued_boottime_nanoseconds: String }
impl RuntimeReadyObservation {
    pub(super) fn issue_on_original_client(client: &Arc<RuntimeImageClient>) -> Result<Self> {
        // Called exactly once in the private actual client factory AFTER all
        // original receipt/cutoff checks. No public/raw/JSON constructor exists.
        let issued=super::super::boottime_stamp()?;
        original_receipt_window(&client.ready.data.request.constraint,&client.ready.data.root_ack_observed_boottime_nanoseconds,&issued)?;
        ensure!(decimal(&issued)? >= decimal(&client.ready.native_received_boottime_nanoseconds)?,"runtime observation clock reversed before issue");
        ensure!(client.ready.observation_issued.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_ok(),
            "runtime ready original observation already issued; no ACK readback reconstruction");
        Ok(Self { client: Arc::clone(client), issued_boottime_nanoseconds: issued })
    }
    pub(in crate::root_storage_bootstrap) fn assert_original_qualification_window_off_app(&self) -> Result<()> {
        self.assert_current()?; self.client.ready.original_receipt_current()
    }
    pub(in crate::root_storage_bootstrap) fn checked_original_qualification_export_off_app(&self) -> Result<RuntimeReadyObservationData> {
        self.assert_original_qualification_window_off_app()?;
        let data = self.checked_data_snapshot_off_app()?;
        self.assert_original_qualification_window_off_app()?; Ok(data)
    }
    /// Pure pointer matching, not an authority grant or an IO/currentness check.
    pub fn matches_original_client(&self, client: &Arc<RuntimeImageClient>) -> bool { Arc::ptr_eq(&self.client, client) }
    /// OFF App. This checks original physical client/registration/config/service
    /// custody and immutable witness. ACK/mount-ID remain their historical fact;
    /// this does not ping FUSE, certify current reads, helper or thread drain.
    pub fn assert_current(&self) -> Result<()> { self.client.current()?; self.client.ready.current() }
    /// OFF App. Current kernel check plus explicitly labeled DATA. Elapsed
    /// enable time never creates another clock, owner, fresh ACK or grant.
    pub fn checked_data_snapshot_off_app(&self) -> Result<RuntimeReadyObservationData> {
        self.assert_current()?;
        let origin = &self.client.ready; let d = &origin.data;
        Ok(RuntimeReadyObservationData { format_version: 1, kind: "runtime-image-ready-observation-data-v1",
            scope: "original-enable-ack-and-current-original-client-only", request_nonce: d.request.request_nonce.clone(),
            bootstrap_nonce: d.request.bootstrap_nonce.clone(), engine_instance_id: d.ack.engine_instance_id.clone(),
            epoch: d.ack.epoch.clone(), mount_id_data: d.ack.mount_id.to_string(),
            original_enable_constraint: d.request.constraint.clone(),
            root_ack_observed_boottime_nanoseconds: d.root_ack_observed_boottime_nanoseconds.clone(),
            native_received_boottime_nanoseconds: origin.native_received_boottime_nanoseconds.clone(),
            native_issued_boottime_nanoseconds: self.issued_boottime_nanoseconds.clone(),
            client_current_checked_boottime_nanoseconds: super::super::boottime_stamp()?,
            registration_proof_sha256: d.registration_proof_sha256.clone(), config_sha256: d.config_sha256.clone(),
            boot_id: d.boot_id.clone(), service_pid_data: d.service_pid,
            inventory: d.ack.inventory.clone(), manifest: d.ack.manifest.clone(),
            installer_input: d.ack.installer_input.clone(), selected_executables: d.ack.selected_executables.clone(),
            source_authority: false, helper_enrolled: false, scheduler_ready: false, normal_retirement_authority: false })
    }
}

#[cfg(test)]
#[path = "root_runtime_ready_tests.rs"]
mod tests;
