//! Original-attempt recovery. Workspace locking is necessary but insufficient:
//! the old driver may outlive its host and Buck2 may escape its process group.
//! Never use a new build/control as evidence that those original effects ended.
use super::*;
use super::frontend::{atomic, bounded};
use std::{fs, io, path::{Path, PathBuf}, process::Command, time::{Duration, Instant}};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ProcessIdentity {
    pid: i32,
    start: u64,
    session: i32,
    boot: String,
}

struct ProcStat { state: char, group: i32, identity: ProcessIdentity }
fn proc_stat(pid: i32) -> Result<Option<ProcStat>> {
    let raw = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let fields: Vec<_> = raw[raw.rfind(')').context("invalid build process stat")? + 2..].split_whitespace().collect();
    ensure!(fields.len() > 19, "short build process stat");
    Ok(Some(ProcStat { state: fields[0].chars().next().context("missing process state")?, group: fields[2].parse()?,
        identity: ProcessIdentity { pid, session: fields[3].parse()?, start: fields[19].parse()?,
            boot: fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().into() } }))
}
fn process_ids() -> Result<Vec<i32>> {
    Ok(fs::read_dir("/proc")?.map(|entry| entry.map(|e|e.file_name().to_string_lossy().parse::<i32>().ok()))
        .collect::<io::Result<Vec<_>>>()?.into_iter().flatten().collect())
}
impl ProcessIdentity {
    pub(super) fn capture(pid: u32) -> Result<Self> {
        let pid = i32::try_from(pid)?;
        Ok(proc_stat(pid)?.context("original build process disappeared before recording its identity")?.identity)
    }
    fn stopped(&self) -> Result<bool> {
        Ok(proc_stat(self.pid)?.is_none_or(|s| s.identity != *self || matches!(s.state, 'Z' | 'X')))
    }
    fn stop_group(&self) -> Result<()> {
        if fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim() != self.boot { return Ok(()); }
        let leader_current = if let Some(now) = proc_stat(self.pid)? {
            ensure!(now.identity == *self, "original driver PID was reused; retain original custody");
            ensure!(now.group == self.pid, "original driver lost its recorded process group");
            true
        } else { false };
        for pid in process_ids()? {
            if let Some(now) = proc_stat(pid)? {
                if now.group == self.pid && !matches!(now.state, 'Z' | 'X') {
                    // A numeric group ID can be reused after its leader exits.
                    // Without the original leader identity, retain custody
                    // instead of signalling a potentially unrelated group.
                    ensure!(leader_current, "original driver is absent but group custody is unresolved");
                    ensure!(now.identity.session == self.session && now.identity.start >= self.start,
                        "original driver group identity changed; retain original custody");
                    let result = unsafe { libc::kill(-self.pid, libc::SIGKILL) };
                    if result != 0 {
                        ensure!(io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH), "original driver group shutdown failed");
                    }
                    break;
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut live = false;
            for pid in process_ids()? {
                if let Some(now) = proc_stat(pid)? {
                    live |= now.group == self.pid && !matches!(now.state, 'Z' | 'X');
                }
            }
            if !live && self.stopped()? { return Ok(()); }
            ensure!(Instant::now() < deadline, "original driver group termination unresolved");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Publication {
    pub contract: ActionContract,
    pub outputs: ObjectRef,
    pub trace_id: String,
    pub run_receipt: Option<ObjectRef>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct InflightBuild {
    format_version: u32,
    pub attempt: String,
    source: ObjectRef,
    environment: ObjectRef,
    retained_input_pin: String,
    checkpoint: ObjectRef,
    owner: ProcessIdentity,
    driver: Option<ProcessIdentity>,
    daemons: Vec<ProcessIdentity>,
    processes_settled: bool,
    pub publication: Option<Publication>,
    reconciliation: Option<Value>,
}
impl InflightBuild {
    pub(super) fn reconciliation_data(&self) -> Option<&Value> { self.reconciliation.as_ref() }
    pub(super) fn new(input: &BuildInputView, environment: &ObjectRef, pin: &str) -> Result<Self> {
        Ok(Self { format_version: 2, attempt: uuid::Uuid::new_v4().to_string(), source: input.source_root().clone(),
            environment: environment.clone(), retained_input_pin: pin.into(), checkpoint: input.checkpoint().clone(),
            owner: ProcessIdentity::capture(std::process::id())?, driver: None, daemons: vec![], processes_settled: false,
            publication: None, reconciliation: None })
    }
    pub(super) fn gate(&self, workspace: &Path) -> PathBuf { workspace.join(format!(".driver-start-{}", self.attempt)) }
    pub(super) fn save(&self, workspace: &Path) -> Result<()> {
        atomic(&workspace.join(".rudder-build-inflight"), &canonical_json(self)?)
    }
    pub(super) fn start_driver(&mut self, workspace: &Path, pid: u32) -> Result<()> {
        ensure!(self.driver.is_none(), "original build driver already recorded");
        self.driver = Some(ProcessIdentity::capture(pid)?);
        self.save(workspace)?;
        atomic(&self.gate(workspace), b"start")
    }
    // The managed driver gives Buck2 this private HOME. It can reuse an idle
    // daemon from a prior successful attempt, whose attempt tag stays old.
    // Inspect only process identity and that workspace binding, never expose environment
    // bytes (which could contain credentials from unrelated processes).
    fn workspace_daemons(workspace: &Path, executable: &Path) -> Result<Vec<ProcessIdentity>> {
        let home = format!("HOME={}", workspace.join(".driver/home").display()).into_bytes();
        let mut found = vec![];
        for pid in process_ids()? {
            let bytes = match fs::read(format!("/proc/{pid}/environ")) {
                Ok(raw) => raw,
                Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => continue,
                // Other nondumpable services can share the host UID. Exclude
                // them by their executable command before treating an unreadable
                // environment as unknown Buck2 custody. Never print arguments.
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
                    let metadata = match fs::metadata(format!("/proc/{pid}")) {
                        Ok(v) => v, Err(e) if e.kind() == io::ErrorKind::NotFound => continue, Err(e) => return Err(e.into()),
                    };
                    if metadata.uid() == unsafe { libc::geteuid() } {
                        let command = match fs::read(format!("/proc/{pid}/cmdline")) {
                            Ok(v) => v,
                            Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => continue,
                            Err(_) => anyhow::bail!("cannot identify a possible original build daemon"),
                        };
                        let name = Path::new(std::ffi::OsStr::from_bytes(command.split(|b|*b==0).next().unwrap_or_default())).file_name();
                        let possible = name == executable.file_name() || name.is_some_and(|v|v.as_bytes().starts_with(b"buck2"));
                        ensure!(!possible, "cannot reconcile an original build daemon environment");
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if bytes.split(|b| *b == 0).any(|v| v == home) {
                if let Some(now) = proc_stat(pid)? {
                    if !matches!(now.state, 'Z' | 'X') { found.push(now.identity); }
                }
            }
        }
        Ok(found)
    }
    pub(super) fn settle_processes(&mut self, workspace: &Path, executable: &Path) -> Result<()> {
        if self.processes_settled { return Ok(()); }
        let driver = self.driver.as_ref().context("original driver identity absent; retain inflight custody")?;
        driver.stop_group()?;
        // No original driver/client remains to start a daemon after this scan.
        for identity in Self::workspace_daemons(workspace, executable)? {
            if !self.daemons.contains(&identity) { self.daemons.push(identity); }
        }
        self.save(workspace)?;
        Self::kill_daemons(workspace, executable)?;
        Self::wait_for_daemons(workspace, executable, &self.daemons)?;
        self.processes_settled = true;
        self.save(workspace)
    }
    pub(super) fn stop_workspace_daemons(workspace: &Path, executable: &Path) -> Result<()> {
        let inflight = workspace.join(".rudder-build-inflight");
        if inflight.try_exists()? {
            let mut original: Self = serde_json::from_slice(&fs::read(inflight)?)
                .context("original inflight identity unavailable; stop requires original reconciliation")?;
            ensure!(original.format_version == 2, "unknown original inflight record");
            // The caller holds the workspace lock. A surviving old driver
            // cannot be allowed to recreate its daemon after a stop client
            // exits. This only settles processes; it never releases the pin.
            return original.settle_processes(workspace, executable);
        }
        let actual = Self::workspace_daemons(workspace, executable)?;
        Self::kill_daemons(workspace, executable)?;
        Self::wait_for_daemons(workspace, executable, &actual)
    }
    fn kill_daemons(workspace: &Path, executable: &Path) -> Result<()> {
        let mut kill = Command::new(executable);
        kill.arg("kill").current_dir(workspace).env_clear().env("PATH", "/usr/bin:/bin")
            .env("HOME", workspace.join(".driver/home"));
        bounded(&mut kill, Duration::from_secs(15), &workspace.join(".original-kill.stdout"), &workspace.join(".original-kill.stderr"))
            .context("original Buck2 daemon shutdown unresolved")
    }
    fn wait_for_daemons(workspace: &Path, executable: &Path, actual: &[ProcessIdentity]) -> Result<()> {
        // A successful replacement client is not termination evidence. Check
        // the actual recorded original identities and the private attempt.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut stopped = Self::workspace_daemons(workspace, executable)?.is_empty();
            for identity in actual { stopped &= identity.stopped()?; }
            if stopped { break; }
            ensure!(Instant::now() < deadline, "original Buck2 daemon remains live after stop; retain inflight custody");
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
    fn reconcile_publication(&mut self, index: &BuildIndex, verify_closures: bool) -> Result<()> {
        let Some(candidate) = &self.publication else { return Ok(()); };
        let store = index.store();
        let key = candidate.contract.key(store)?;
        let retained = store.retention()?;
        let action = retained.pins.get(&format!("build/action/{}", key.digest));
        // Cache admission may be closed while an old publication still exists.
        // Reconcile that fact without making its result reusable.
        let indexed = index.retained_action_for_reconciliation(&candidate.contract)?;
        let action_trace = if let Some(receipt) = action {
            #[derive(Deserialize)] struct Data { key: ObjectRef, outputs: ObjectRef, observation: FrontendObservation }
            let actual: Data = decode_payload(store, receipt, "frontend-build-receipt-v1")?;
            ensure!(actual.key == key && actual.outputs == candidate.outputs, "original action publication differs from retained candidate");
            if verify_closures { store.closure(receipt)?; }
            Some(actual.observation.trace_id)
        } else { None };
        let run = retained.pins.get(&format!("build/run/{}", candidate.trace_id));
        if let Some(receipt) = run {
            ensure!(candidate.run_receipt.as_ref() == Some(receipt), "original run publication differs from retained candidate");
            if verify_closures { store.closure(receipt)?; } else { store.read_object(receipt)?; }
        }
        self.reconciliation = Some(serde_json::json!({"attempt":self.attempt,"action":action,"action_trace_id":action_trace,
            "indexed":indexed,"run":run,"original_trace_id":candidate.trace_id}));
        ensure!(action == indexed.as_ref(), "original action pin/index publication is partial; retain recovery custody");
        Ok(())
    }
    fn validate_custody(&self, index: &BuildIndex, verify_closure: bool) -> Result<()> {
        let suffix = self.retained_input_pin.strip_prefix("build/input/").context("invalid retained build input pin")?;
        uuid::Uuid::parse_str(suffix)?;
        let store = index.store();
        ensure!(store.retention()?.pins.get(&self.retained_input_pin) == Some(&self.checkpoint), "original retained input pin/checkpoint changed");
        // Validate the ORIGINAL checkpoint, never the next build's input.
        let actual = super::sealed_inputs::qualified_input(store, &self.checkpoint)?;
        ensure!(actual.source_root() == &self.source && actual.base_environment() == Some(&self.environment),
            "original retained checkpoint source/environment changed");
        if verify_closure { store.closure(&self.checkpoint)?; }
        Ok(())
    }
    pub(super) fn finish(&mut self, index: &BuildIndex, workspace: &Path, executable: &Path) -> Result<()> {
        self.settle_processes(workspace, executable)?;
        ensure!(self.processes_settled, "original build processes not settled");
        let reconciliation = self.reconcile_publication(index, true);
        // Preserve classified partial publication data even when unresolved.
        self.save(workspace)?;
        reconciliation?;
        self.release_custody(index, workspace, true)
    }
    /// The original synchronous frontend returned verified success. Transfer
    /// the checkpoint into its OWN durable run receipt, which references that
    /// checkpoint, before dropping the temporary input pin. An idle warm Buck2
    /// daemon may remain; no daemon-termination claim is made by this path.
    /// Errors/crashes never use this success-only transfer as recovery proof.
    pub(super) fn finish_published(&mut self, index: &BuildIndex, workspace: &Path, build: &BuildRun) -> Result<()> {
        ensure!(self.driver.as_ref().context("original driver absent")?.stopped()?, "original successful driver remains live");
        let candidate = self.publication.as_ref().context("original publication candidate absent")?;
        ensure!(candidate.run_receipt.as_ref() == Some(&build.run_receipt)
            && candidate.trace_id == build.observation.trace_id && candidate.outputs == build.stored.outputs,
            "original synchronous result differs from publication candidate");
        let pins = index.store().retention()?.pins;
        ensure!(pins.get(&format!("build/run/{}", candidate.trace_id)) == Some(&build.run_receipt)
            && pins.get(&format!("build/action/{}", build.stored.key.digest)) == Some(&build.stored.receipt),
            "original successful publication lacks durable custody");
        let receipt: ReferencedMetadata = serde_json::from_slice(&index.store().read_object(&build.run_receipt)?)?;
        ensure!(receipt.references.contains(&self.checkpoint)
            && receipt.payload["schema"] == "managed-build-run-v1"
            && receipt.payload["value"]["input_view"]["checkpoint"] == serde_json::to_value(&self.checkpoint)?
            && receipt.payload["value"]["stored"] == serde_json::to_value(&build.stored.receipt)?,
            "original run receipt did not retain its exact checkpoint/result");
        // These publications already verified their closures. Do not rehash
        // the entire compiler image on every warm build's retention transfer.
        self.reconcile_publication(index, false)?;
        self.release_custody(index, workspace, false)
    }
    fn release_custody(&self, index: &BuildIndex, workspace: &Path, verify_closure: bool) -> Result<()> {
        self.validate_custody(index, verify_closure)?;
        index.store().release_build_input_pin(&self.retained_input_pin, &self.checkpoint)?;
        fs::remove_file(workspace.join(".rudder-build-inflight"))?;
        let _ = fs::remove_file(self.gate(workspace));
        Ok(())
    }
    pub(super) fn recovery(index: &BuildIndex, workspace: &Path, executable: &Path) -> Result<()> {
        let path = workspace.join(".rudder-build-inflight");
        if !path.try_exists()? { return Ok(()); }
        let mut original: Self = serde_json::from_slice(&fs::read(&path)?)
            .context("original inflight identity unavailable; retain marker and input pin")?;
        ensure!(original.format_version == 2 && uuid::Uuid::parse_str(&original.attempt).is_ok(), "unknown original inflight record");
        ensure!(original.owner.stopped()?, "original build owner is still live; retain original custody");
        original.finish(index, workspace, executable)
            .context("original inflight reconciliation unresolved; retain marker and input pin")
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "inflight_tests.rs"]
mod tests;
