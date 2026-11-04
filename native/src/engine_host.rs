//! An explicit extraction seam: the existing single-writer App can outlive its
//! terminal views. This is one project owner, not yet the multi-project engine.
use super::*;
use fs2::FileExt;
use rudder_engine::journal::{
    HeldJournalFile, Journal, OwnerFence, ReadOnlyJournal,
};
use rudder_engine::paths::{local_project_id, FabricPaths};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::BufReader;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const ENGINE_FORMAT: u32 = 1;
const CONTROL_LIMIT: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeAuthority {
    Legacy,
    Fabric,
}

impl NativeAuthority {
    fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy-native",
            Self::Fabric => "fabric",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedFence {
    project_id: String,
    owner_id: String,
    epoch: u64,
}

impl From<&OwnerFence> for RecordedFence {
    fn from(value: &OwnerFence) -> Self {
        Self {
            project_id: value.project_id.clone(),
            owner_id: value.owner_id.clone(),
            epoch: value.epoch,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerRecord {
    format_version: u32,
    repo: PathBuf,
    authority: String,
    implementation: String,
    pid: u32,
    owner: RecordedFence,
    instance_id: String,
    attach_socket: PathBuf,
    control_socket: Option<PathBuf>,
    state: String,
    started_at: String,
    control_nonce: Option<String>,
}

impl OwnerRecord {
    fn public(&self, running: bool) -> serde_json::Value {
        serde_json::json!({
            "format_version": self.format_version, "running": running,
            "repo": self.repo, "authority": self.authority,
            "implementation": self.implementation, "pid": self.pid,
            "owner": self.owner, "instance_id": self.instance_id,
            "attach_socket": self.attach_socket, "control_socket": self.control_socket,
            "state": self.state, "started_at": self.started_at,
        })
    }
}

/// The OS lock fences compatible native owners, including legacy mode. The
/// journal CAS supplies the persisted epoch. A PID is informational only.
pub(crate) struct EngineOwnership {
    lifetime: Arc<EngineOwnerLifetime>,
    journal: Journal,
    fence: OwnerFence,
    record: OwnerRecord,
    record_path: PathBuf,
    released: bool,
    identity_home: PathBuf,
    managed_store: Option<crate::managed_store::ManagedStore>,
}

/// Private, actual process-local custody. Only successful EngineOwnership
/// acquisition constructs this value; saved projections cannot recreate it.
struct EngineOwnerLifetime {
    lock: Arc<CheckoutLock>,
    #[cfg(target_os="linux")]
    assigned:Option<Arc<crate::managed_fresh_assigned::AssignedFreshAuthority>>,
    repo: PathBuf,
    owner: OwnerFence,
    authority: NativeAuthority,
    process_id: u32,
    revoked: AtomicBool,
}

#[derive(Debug)]
pub(crate) struct EngineSharedAdmissionBusy;
impl std::fmt::Display for EngineSharedAdmissionBusy{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str("actual shared admission replacement busy")}}
impl std::error::Error for EngineSharedAdmissionBusy{}
impl EngineOwnerLifetime {
    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    fn check_active(&self) -> Result<()> {
        anyhow::ensure!(
            !self.revoked.load(Ordering::Acquire),
            "native owner lifetime is retired"
        );
        if self.process_id != std::process::id() {
            self.revoke();
            bail!("native owner lifetime belongs to another process");
        }
        Ok(())
    }

    fn check_shared_admission(&self)->Result<()> {self.check_active()?;
        #[cfg(target_os="linux")] if let Some(assigned)=&self.assigned{
            use rudder_native::managed_owner_lease::ManagedOwnerAdmission;
            if assigned.admission_state()==ManagedOwnerAdmission::Busy{return Err(EngineSharedAdmissionBusy.into());}
            if let Err(error)=assigned.check_active(){return if assigned.admission_state()==ManagedOwnerAdmission::Fenced{Err(error)}else{Err(EngineSharedAdmissionBusy.into())};}
        }Ok(())}
    fn check_custody(&self) -> Result<()> {
        let result = (|| -> Result<()> {
            self.check_active()?;
            #[cfg(test)] tests::custody_contention::restriction()?;
            #[cfg(target_os="linux")] if let Some(assigned)=&self.assigned{assigned.assert_retained_custody()?;}
            let held = self.lock.metadata()?;
            let named = fs::symlink_metadata(self.repo.join(".rudder/engine/owner.lock"))?;
            anyhow::ensure!(
                held.is_file()
                    && named.file_type().is_file()
                    && held.dev() == named.dev()
                    && held.ino() == named.ino()
                    && held.nlink() == 1
                    && named.nlink() == 1
                    && held.uid() == unsafe { libc::geteuid() },
                "native owner checkout lock custody changed"
            );
            self.check_active()
        })();
        if result.as_ref().is_err_and(|error|!custody_busy(error)) { self.revoke(); }
        result
    }
}
fn custody_busy(error:&anyhow::Error)->bool{error.downcast_ref::<EngineSharedAdmissionBusy>().is_some()}

/// Host-held local owner lifetime only, with no serialized constructor. The
/// actual checkout lock remains held until every outstanding lease is dropped,
/// including after retirement. Retirement revokes checks immediately; retaining
/// this lease never retains a Journal/run grant or source/history acceptance.
#[derive(Clone)]
pub(crate) struct EngineSourceLease {
    // Close the actual Journal anchor descriptors before the final lock holder.
    database: Arc<HeldJournalFile>,
    journal_identity: (u64, u64),
    lifetime: Arc<EngineOwnerLifetime>,
}

impl EngineSourceLease {
    /// Off-App. Validate the supplied writer's actual SQLite connection before
    /// comparing its inode with the anchor minted by this owner. A matching
    /// filename or copied database is insufficient; neither check opens an
    /// independent raw SQLite file descriptor.
    pub(crate) fn check_journal(&self, journal: &Journal) -> Result<()> {
        self.check_current()?;
        let supplied = journal.hold_database_file()?;
        let metadata = std::fs::symlink_metadata(journal.database_path()?)?;
        anyhow::ensure!(
            (metadata.dev(), metadata.ino()) == self.journal_identity,
            "source preparation Journal differs from the actual owner connection"
        );
        supplied.check_current()?;
        self.check_current()
    }

    /// App-safe process-local retirement check only. Fresh database authority
    /// and held-file custody must have been checked by the actual join worker.
    pub(crate) fn check_active_for_adoption(&self) -> Result<()> {
        self.lifetime.check_shared_admission()
    }
    pub(crate) fn owner(&self) -> &OwnerFence {
        &self.lifetime.owner
    }

    /// Off-App filesystem checks of local held custody, not database authority.
    /// The later source adapter must independently authorize the exact current
    /// Journal binding using a bounded connection and fresh trusted time.
    pub(crate) fn check_current(&self) -> Result<()> {
        let result = (|| {
            self.lifetime.check_custody()?;
            self.database.check_current()?;
            self.lifetime.check_active()
        })();
        if result.as_ref().is_err_and(|error|!custody_busy(error)) { self.lifetime.revoke(); }
        result
    }

    /// Actual assigned lease alias for task-only host consumers. None is an
    /// actual legacy lifetime; paths/identity JSON cannot populate this slot.
    #[cfg(target_os="linux")]
    pub(crate) fn managed_owner_lease(&self)->Option<&Arc<rudder_native::managed_owner_lease::ManagedOwnerLease>> {self.lifetime.assigned.as_ref().map(|owner|owner.lease())}

    pub(crate) fn shared_admission_closed(&self)->bool {
        #[cfg(target_os="linux")] if let Some(assigned)=&self.lifetime.assigned{return assigned.admission_state()==rudder_native::managed_owner_lease::ManagedOwnerAdmission::Fenced;}
        false
    }

    /// New shared work/adoption, distinct from original-resource settlement.
    /// Local held custody remains available after PG expiry for typed cleanup.
    pub(crate) fn check_shared_current(&self)->Result<()> {self.check_current()?;self.lifetime.check_shared_admission()?;#[cfg(target_os="linux")] if let Some(assigned)=&self.lifetime.assigned{assigned.assert_custody()?;}self.lifetime.check_shared_admission()}

    pub(crate) fn wait_shared_current(&self,budget:&rudder_engine::execution::finalization_budget::FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        loop{budget.remaining()?;anyhow::ensure!(!cancelled.load(Ordering::Acquire),"original shared validation cancelled");match self.check_shared_current(){Ok(())=>{budget.remaining()?;return Ok(());},Err(error) if error.downcast_ref::<EngineSharedAdmissionBusy>().is_some()=>std::thread::sleep(budget.phase_limit(Duration::from_millis(2))?),Err(error)=>return Err(error)}}
    }

    /// Off-App, bounded existing read-only Journal derived from this actual
    /// EngineOwnership connection. No caller-supplied database path is accepted.
    pub(crate) fn open_journal(&self, busy_timeout: Duration) -> Result<ReadOnlyJournal<'_>> {
        self.check_current()?;
        let result = self.database.open_read_only(busy_timeout);
        self.check_current()?;
        result
    }

    /// Off-App existing writer, anchored to the actual owner's held database.
    /// Opening it creates neither a project nor a scheduler or source grant.
    pub(crate) fn open_source_writer(&self) -> Result<Journal> {
        self.check_current()?;
        let journal = self.database.open_existing_writer(Duration::from_millis(100))?;
        self.check_journal(&journal)?;
        let state = journal.project_state(&self.lifetime.owner.project_id).map_err(|error| {
            self.lifetime.revoke();
            error
        })?;
        if !state.owner_active || state.owner != self.lifetime.owner {
            self.lifetime.revoke();
            bail!("owner-bound App writer belongs to a retired epoch");
        }
        self.check_current()?;
        Ok(journal)
    }
}

impl EngineOwnership {
    /// Off-App bootstrap. Captures actual held owner custody, never a path from
    /// an agent request. The source worker opens its own bounded connection.
    #[cfg(target_os = "linux")]
    pub(crate) fn source_journal_factory(&self, expected: &OwnerFence)
        -> Result<crate::managed_source_app::SourceJournalFactory> {
        let lease = self.source_lease(expected)?;
        Ok(Box::new(move || lease.open_source_writer()))
    }
    pub(crate) fn acquire(repo: &Path, mode: NativeAuthority) -> Result<Self> {
        Self::acquire_inner(repo, mode, None, None)
    }

    /// Off-App, explicit local identity. Existing launchers retain their home
    /// selection; managed preparation/recovery never infer a different home.
    pub(crate) fn acquire_in_home(repo: &Path, mode: NativeAuthority, home: &Path) -> Result<Self> {
        Self::acquire_inner(repo, mode, Some(home), None)
    }

    /// Explicit prepare recovery, not launch/resume permission. The caller
    /// supplies the exact historical request; incomplete evidence is retained.
    pub(crate) fn recover_managed_prepare(repo: &Path, home: &Path,
        request: &rudder_engine::journal::ManagedProjectPrepare) -> Result<Self> {
        let mut owner = Self::acquire_inner(repo, NativeAuthority::Fabric, Some(home), Some(request))?;
        owner.prepare_managed_project(request)?;
        Ok(owner)
    }
    pub(crate) fn recover_managed_prepare_with_plan(repo: &Path, home: &Path,
        request: &rudder_engine::journal::ManagedProjectPrepare, plan: &crate::managed_store::ExperimentalRecoveryPlan) -> Result<Self> {
        let mut owner=Self::acquire_inner(repo,NativeAuthority::Fabric,Some(home),Some(request))?;
        owner.prepare_managed_project_with_plan(request,plan)?; Ok(owner)
    }

    fn acquire_inner(repo: &Path, mode: NativeAuthority, explicit_home: Option<&Path>,
        recovery: Option<&rudder_engine::journal::ManagedProjectPrepare>) -> Result<Self> {
        let repo = repo.canonicalize().context("canonical engine checkout")?;
        // Presence is conservative refusal evidence only. The actual fresh
        // constructor requires original registered/PG/namespace/lease custody.
        match fs::symlink_metadata(repo.join(".rudder/engine/fresh-project.json")){Ok(_)=>bail!("fresh logical managed checkout requires its original assigned owner; legacy/default acquisition refused"),Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}
        let directory = repo.join(".rudder/engine");
        let lock = acquire_checkout_lock(&repo)?;
        let attach = repo.join(".rudder").join(ATTACH_SOCKET_NAME);
        // Old native versions do not hold our lock. Never take over one that
        // still answers its established attach socket.
        if attach_is_live(&attach)? {
            bail!("a live legacy dashboard already owns this checkout; close it explicitly before starting another owner");
        }
        if mode == NativeAuthority::Fabric {
            reject_legacy_writers(&repo)?;
        }
        let home = explicit_home.map(Path::to_path_buf).or_else(signals::rudder_home)
            .context("no Rudder home for the engine journal")?;
        let paths = FabricPaths::new(&home);
        let mut new_directories=Vec::new();
        for ancestor in paths.root.ancestors() {
            let ancestor=if ancestor.as_os_str().is_empty() { Path::new(".") } else { ancestor };
            if ancestor.try_exists()? {
                if !new_directories.is_empty() { new_directories.push(ancestor.to_path_buf()); }
                break;
            }
            anyhow::ensure!(new_directories.len()<64,"engine home directory creation exceeds bounded depth");
            new_directories.push(ancestor.to_path_buf());
        }
        fs::create_dir_all(&paths.root)?;
        fs::set_permissions(&paths.root, fs::Permissions::from_mode(0o700))?;
        // Persist every newly created directory entry, including a new HOME's
        // entry in its first existing ancestor, before publishing any floor.
        for directory in new_directories { File::open(directory)?.sync_all()?; }
        let canonical_home = home.canonicalize()?;
        let canonical_fabric = paths.root.canonicalize()?;
        let private_root = if canonical_home != repo {
            &canonical_home
        } else {
            &canonical_fabric
        };
        if let Ok(relative) = private_root.strip_prefix(&repo) {
            // A deliberately relocated Rudder home must not snapshot its live
            // journal/WAL or private config into an agent's source workspace.
            let mut pattern = String::from("/");
            for ch in relative.to_string_lossy().chars() {
                if matches!(ch, '\\' | '[' | ']' | '*' | '?' | '#' | '!' | ' ') {
                    pattern.push('\\');
                }
                pattern.push(ch);
            }
            pattern.push('/');
            exclude_runtime_path(&repo, &pattern)?;
        }
        let paths = FabricPaths::new(&canonical_home);
        let mut journal = Journal::open(&paths.journal)?;
        let project = local_project_id(&repo)?;
        // This check precedes every owner transfer, recovery operation and
        // owner.json rewrite. Intent alone blocks a legacy restart after a
        // crash before the schema5 transaction; a floor alone blocks one after.
        let managed = journal.managed_project(&project)?;
        let namespace_present = crate::managed_store::has_namespace_evidence(&canonical_home, &project)?;
        if let Some(request) = recovery {
            let binding = crate::managed_store::namespace(&canonical_home, &repo, &project, &journal)?;
            anyhow::ensure!(request.namespace == binding
                && crate::managed_store::read_prepare_intent(&binding, &journal)? == *request
                && managed.as_ref().is_none_or(|receipt|receipt.request == *request),
                "managed recovery request differs from retained evidence");
        } else {
            anyhow::ensure!(managed.is_none() && !namespace_present,
                "managed project is prepared or quarantined; legacy Native startup is refused before ownership changes; use explicit recovery");
        }
        let instance_id = uuid::Uuid::new_v4().to_string();
        let owner_id = format!("{}:{instance_id}", mode.name());
        // The lock plus refusal above establishes local quiescence. A transfer
        // is a journal CAS, never a lease timeout or a PID-based takeover.
        let fence = match journal.project_state(&project) {
            Ok(state) => journal.transfer_owner(&state.owner, &owner_id, epoch_ms())?,
            Err(error)
                if matches!(
                    error.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::QueryReturnedNoRows)
                ) =>
            {
                journal.create_project(
                    &project,
                    &owner_id,
                    &initial_engine_head(&repo),
                    epoch_ms(),
                )?
            }
            Err(error) => return Err(error).context("read existing project authority"),
        };
        journal.recover(&fence, epoch_ms())?;
        let record = OwnerRecord {
            format_version: ENGINE_FORMAT,
            repo,
            authority: mode.name().into(),
            implementation: "legacy-native-extraction".into(),
            pid: std::process::id(),
            owner: RecordedFence::from(&fence),
            instance_id,
            attach_socket: attach,
            control_socket: None,
            state: "starting".into(),
            started_at: now_stamp(),
            control_nonce: None,
        };
        let record_path = directory.join("owner.json");
        write_owner_record(&record_path, &record)?;
        let lifetime = Arc::new(EngineOwnerLifetime {
            lock:Arc::new(lock),
            #[cfg(target_os="linux")] assigned:None,
            repo: record.repo.clone(),
            owner: fence.clone(),
            authority: mode,
            process_id: std::process::id(),
            revoked: AtomicBool::new(false),
        });
        Ok(Self {
            lifetime,
            journal,
            fence,
            record,
            record_path,
            released: false,
            identity_home: canonical_home,
            managed_store: None,
        })
    }

    /// Off-App explicit opt-in, while this actual CheckoutLock is held and
    /// before any App/scheduler is started. This does not enable managed launch.
    pub(crate) fn prepare_managed_project(&mut self,
        request: &rudder_engine::journal::ManagedProjectPrepare) -> Result<crate::managed_store::ManagedStore> {
        self.prepare_managed_inner(request,None)
    }
    pub(crate) fn experimental_recovery_plan(&self) -> Result<crate::managed_store::ExperimentalRecoveryPlan> {
        self.assert_current()?;
        let binding=crate::managed_store::namespace(&self.identity_home,&self.record.repo,&self.fence.project_id,&self.journal)?;
        crate::managed_store::inspect_experimental(&binding)
    }
    pub(crate) fn prepare_managed_project_with_plan(&mut self,
        request: &rudder_engine::journal::ManagedProjectPrepare, plan: &crate::managed_store::ExperimentalRecoveryPlan) -> Result<crate::managed_store::ManagedStore> {
        self.prepare_managed_inner(request,Some(plan))
    }
    fn prepare_managed_inner(&mut self, request: &rudder_engine::journal::ManagedProjectPrepare,
        recovery: Option<&crate::managed_store::ExperimentalRecoveryPlan>) -> Result<crate::managed_store::ManagedStore> {
        anyhow::ensure!(self.record.state == "starting" && self.record.authority == "fabric",
            "managed preparation requires a starting owned Fabric engine");
        self.assert_current()?;
        reject_legacy_writers(&self.record.repo)?;
        let binding = crate::managed_store::namespace(&self.identity_home, &self.record.repo,
            &self.fence.project_id, &self.journal)?;
        anyhow::ensure!(request.namespace == binding, "managed prepare differs from the actual owner identity");
        let experimental=crate::managed_store::inspect_experimental(&binding)?;
        anyhow::ensure!(recovery.map_or_else(||experimental.is_empty(),|plan|plan==&experimental),
            "experimental managed records require an explicit exact bounded recovery plan; originals retained");
        crate::managed_store::persist_prepare_intent(request, &self.journal)?;
        if let Some(plan)=recovery { crate::managed_store::persist_recovery_plan(plan,&binding)?; }
        let receipt = self.journal.prepare_managed_project(&self.fence, request, epoch_ms())?;
        let store = crate::managed_store::persist_prepared_receipt(&receipt, &self.journal)?;
        if let Some(plan)=recovery { crate::managed_store::preserve_experimental(&store,plan)?; }
        self.managed_store = Some(store.clone());
        Ok(store)
    }

    pub(crate) fn managed_recovery_store(&self) -> Option<&crate::managed_store::ManagedStore> { self.managed_store.as_ref() }

    pub(crate) fn require_legacy_startup(&self) -> Result<()> {
        anyhow::ensure!(self.managed_store.is_none()
            && self.journal.managed_project(&self.fence.project_id)?.is_none()
            && !crate::managed_store::has_namespace_evidence(&self.identity_home, &self.fence.project_id)?,
            "managed project remains recovery-only; legacy App/scheduler startup is refused");
        Ok(())
    }

    pub(crate) fn assert_current(&self) -> Result<()> {
        self.lifetime.check_shared_admission()?;
        let result = (|| -> Result<()> {
            let state = self.journal.project_state(&self.fence.project_id)?;
            anyhow::ensure!(
                state.owner_active
                    && state.owner.epoch == self.fence.epoch
                    && state.owner.owner_id == self.fence.owner_id,
                "native ownership epoch changed; stopping this owner"
            );
            Ok(())
        })();
        if result.is_err() {
            self.lifetime.revoke();
        }
        result
    }

    /// App-safe actual admission check: original process/clock/atomics only.
    pub(crate) fn check_shared_for_app(&self)->Result<()> {self.lifetime.check_shared_admission()}
    /// Pure permission state only. No saved flag/record can create the actual
    /// lease; this distinguishes ordinary shared fencing from physical failure.
    pub(crate) fn shared_admission_closed(&self)->bool {
        #[cfg(target_os="linux")] if let Some(assigned)=&self.lifetime.assigned{return assigned.admission_state()==rudder_native::managed_owner_lease::ManagedOwnerAdmission::Fenced;}
        false
    }
    pub(crate) fn shared_admission_busy(&self)->bool {
        #[cfg(target_os="linux")] if let Some(assigned)=&self.lifetime.assigned{return assigned.admission_state()==rudder_native::managed_owner_lease::ManagedOwnerAdmission::Busy;}
        false
    }
    pub(crate) fn assert_current_bounded(&self,budget:&rudder_engine::execution::finalization_budget::FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        loop{budget.remaining()?;anyhow::ensure!(!cancelled.load(Ordering::Acquire),"original shared owner validation cancelled");match self.assert_current(){Ok(())=>{budget.remaining()?;return Ok(());},Err(error) if error.downcast_ref::<EngineSharedAdmissionBusy>().is_some()=>std::thread::sleep(budget.phase_limit(Duration::from_millis(2))?),Err(error)=>return Err(error)}}
    }
    /// Original local resource custody for cleanup polling after PG fencing.
    /// This grants no new task/source/head/publication or general writer use.
    pub(crate) fn assert_original_local_custody(&self)->Result<()> {
        self.lifetime.check_custody()?;let held=self.journal.hold_database_file()?;held.check_current()?;let state=self.journal.project_state(&self.fence.project_id)?;
        anyhow::ensure!(state.owner_active&&state.owner==self.fence,"original local owner changed");Ok(())
    }

    /// Off-App. Requires this actual live Fabric owner and an exact expected
    /// fence; it does not issue a source capability. Existing leases survive
    /// graceful stopping until finish, but no new lease is admitted then.
    pub(crate) fn source_lease(&self, expected: &OwnerFence) -> Result<EngineSourceLease> {
        anyhow::ensure!(
            self.lifetime.authority == NativeAuthority::Fabric,
            "host source leases require a Fabric engine owner"
        );
        anyhow::ensure!(
            expected == &self.fence && expected == &self.lifetime.owner,
            "host source lease owner fence mismatch"
        );
        anyhow::ensure!(
            !self.released && matches!(self.record.state.as_str(), "starting" | "ready"),
            "native owner no longer admits host source leases"
        );
        self.assert_current()?;
        let database = self.journal.hold_database_file().map_err(|error| {
            self.lifetime.revoke();
            error
        })?;
        let metadata = std::fs::symlink_metadata(self.journal.database_path()?)?;
        let journal_identity = (metadata.dev(), metadata.ino());
        database.check_current()?;
        let lease = EngineSourceLease {
            lifetime: Arc::clone(&self.lifetime),
            database:Arc::new(database),
            journal_identity,
        };
        lease.check_current()?;
        Ok(lease)
    }

    pub(crate) fn ready(&mut self, control: Option<&Path>) -> Result<serde_json::Value> {
        self.assert_current()?;
        anyhow::ensure!(
            matches!(self.record.state.as_str(), "starting" | "ready"),
            "native owner cannot become ready after stopping"
        );
        self.record.control_socket = control.map(Path::to_path_buf);
        self.record.state = "ready".into();
        if control.is_some() {
            self.record.control_nonce = Some(uuid::Uuid::new_v4().to_string());
        }
        if let Err(error) = write_owner_record(&self.record_path, &self.record) {
            self.lifetime.revoke();
            return Err(error);
        }
        Ok(self.record.public(true))
    }

    pub(crate) fn finish(&mut self, clean: bool) -> Result<()> {
        // Revoke before any fallible persistence or Journal release. Outstanding
        // callbacks retain the lock, but can never revive this owner lifetime.
        self.lifetime.revoke();
        // A fenced App must not overwrite its successor's projection, even to
        // report uncertainty. Release must succeed before advertising stopped.
        self.journal.hold_database_file()?.check_current()?;
        let state = self.journal.project_state(&self.fence.project_id)?;
        anyhow::ensure!(state.owner == self.fence && (state.owner_active || self.released),
            "cannot reconcile a retired native owner over its successor");
        if self.released { return write_owner_record(&self.record_path, &self.record); }
        // First persist stopping, not stopped. A failed projection write keeps
        // authority active for recovery; a failed release never advertises a
        // completed stop. The second projection follows the release receipt.
        self.record.state = if clean { "stopping" } else { "uncertain" }.into();
        write_owner_record(&self.record_path, &self.record)?;
        if clean {
            if let Err(error) = self.journal.release_owner(&self.fence, epoch_ms()) {
                self.record.state = "uncertain".into();
                let _ = write_owner_record(&self.record_path, &self.record);
                return Err(error);
            }
            self.released = true;
            self.record.state = "stopped".into();
            write_owner_record(&self.record_path, &self.record)?;
        }
        Ok(())
    }

    pub(crate) fn app_identity(&self) -> (&Path, &OwnerFence) {
        (&self.record.repo, &self.fence)
    }

    /// Cleanup publication only, including after a startup projection failure
    /// revoked new-work admission. This never revives the owner or a source
    /// lease, and a successor epoch cannot be overwritten by local teardown.
    pub(crate) fn assert_app_cleanup_epoch(&self) -> Result<()> {
        self.journal.hold_database_file()?.check_current()?;
        let state = self.journal.project_state(&self.fence.project_id)?;
        anyhow::ensure!(state.owner_active && state.owner == self.fence,
            "App cleanup belongs to a retired owner epoch");
        Ok(())
    }

    pub(crate) fn is_fabric(&self) -> bool {
        self.record.authority == "fabric"
    }

    pub(crate) fn journal_parts(&mut self) -> (&mut Journal, &OwnerFence) {
        (&mut self.journal, &self.fence)
    }
}

impl Drop for EngineOwnership {
    fn drop(&mut self) {
        // No filesystem/SQLite work here. The last actual lease/owner drop
        // performs CheckoutLock's existing bounded explicit unlock.
        self.lifetime.revoke();
    }
}

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn initial_engine_head(repo: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|value| value.status.success())
        .map(|value| String::from_utf8_lossy(&value.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unversioned".into())
}

fn process_is_live(pid: u64) -> bool {
    if pid == 0 || pid > libc::pid_t::MAX as u64 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn read_legacy_metadata(path: &Path) -> Result<Option<serde_json::Value>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "legacy metadata exceeds the bounded migration scan"
    );
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("invalid legacy metadata at {}", path.display())
    })?))
}

/// Compatible writers take the OS lock. Historical TS workers/board schedulers
/// do not, so an explicit migration also refuses their known live processes.
/// This never signals, deletes or relabels those processes/records. Older
/// clients can still race this preflight; a complete handoff remains a gate.
fn reject_legacy_writers(repo: &Path) -> Result<()> {
    let runs = match fs::read_dir(repo.join(".rudder/runs")) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(entries) = runs {
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            anyhow::ensure!(
                !kind.is_symlink(),
                "cannot establish quiescence of a symlinked legacy run store"
            );
            if !kind.is_dir() {
                continue;
            }
            let Some(value) = read_legacy_metadata(&entry.path().join("run.json"))? else {
                continue;
            };
            if let Some(process) = value.get("process") {
                for field in ["pid", "controllerPid", "backendPid"] {
                    if let Some(pid) = process.get(field).and_then(serde_json::Value::as_u64) {
                        anyhow::ensure!(!process_is_live(pid),
                            "live legacy writer PID {pid} is recorded for run {:?}; stop its owner explicitly before fabric migration", entry.file_name());
                    }
                }
            }
        }
    }
    if let Some(value) = read_legacy_metadata(&repo.join(".rudder/daemon.pid"))? {
        let projector =
            value.get("controlMode").and_then(serde_json::Value::as_str) == Some("projector");
        if !projector {
            if let Some(pid) = value.get("pid").and_then(serde_json::Value::as_u64) {
                anyhow::ensure!(!process_is_live(pid),
                    "live legacy scheduler PID {pid} owns this project; stop it explicitly before fabric migration");
            }
        }
    }
    Ok(())
}

fn exclude_runtime_path(repo: &Path, pattern: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    let path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    let path = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line == pattern) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    writeln!(file, "{pattern}")?;
    Ok(())
}

fn connect_local(path: PathBuf) -> io::Result<UnixStream> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("rudder-local-connect".into())
        .spawn(move || {
            let _ = tx.send(UnixStream::connect(path));
        })?;
    rx.recv_timeout(Duration::from_millis(500)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "local owner socket connection timed out",
        )
    })?
}

/// Check filesystem absence before constructing a Unix socket address: Darwin
/// rejects a long checkout path before connect() can report ENOENT. Live attach
/// sockets live at short targets behind the checkout's symlink.
fn existing_attach_endpoint(path: &Path) -> io::Result<Option<PathBuf>> {
    let mut endpoint = path.to_path_buf();
    for _ in 0..40 {
        let metadata = match fs::symlink_metadata(&endpoint) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&endpoint)?;
            endpoint = if target.is_absolute() {
                target
            } else {
                endpoint.parent().unwrap_or_else(|| Path::new(".")).join(target)
            };
            continue;
        }
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing attach endpoint is not a Unix socket",
            ));
        }
        if endpoint.as_os_str().len() >= 100 {
            // A relative symlink can contain enough ../ components to reach a
            // short socket even though its joined spelling is long. Preserve
            // already-short aliases such as /tmp on macOS.
            match endpoint.canonicalize() {
                Ok(shorter) if shorter.as_os_str().len() < endpoint.as_os_str().len() => {
                    endpoint = shorter;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        return Ok(Some(endpoint));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "attach endpoint contains too many symbolic links",
    ))
}

fn attach_is_live(path: &Path) -> Result<bool> {
    let endpoint = existing_attach_endpoint(path)
        .context("cannot establish that the previous attach owner is quiescent")?;
    let Some(endpoint) = endpoint else { return Ok(false) };
    match connect_local(endpoint) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(false)
        }
        Err(error) => {
            Err(error).context("cannot establish that the previous attach owner is quiescent")
        }
    }
}

fn write_owner_record(path: &Path, value: &OwnerRecord) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    format_version: u32,
    op: String,
    instance_id: String,
    epoch: u64,
    nonce: String,
}

impl ControlRequest {
    fn authenticates(&self, owner: &OwnerRecord) -> bool {
        self.format_version == ENGINE_FORMAT
            && self.instance_id == owner.instance_id
            && self.epoch == owner.owner.epoch
            && owner.control_nonce.as_deref() == Some(self.nonce.as_str())
    }
}

struct ControlEvent {
    request: ControlRequest,
    response: mpsc::SyncSender<serde_json::Value>,
}

fn prepare_control_stream(stream: &UnixStream) -> io::Result<()> {
    // Darwin inherits a nonblocking listener's flag. The background control
    // worker uses bounded blocking IO for complete request/response frames.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))
}

pub(crate) struct EngineControl {
    path: PathBuf,
    identity: (u64, u64),
    rx: mpsc::Receiver<ControlEvent>,
    stopping: bool,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EngineControl {
    pub(crate) fn bind(owner: &EngineOwnership, waker: Option<PtyOutputWaker>) -> Result<Self> {
        let home = signals::rudder_home().context("no Rudder home for engine control")?;
        let hash = owner
            .record
            .owner
            .project_id
            .strip_prefix("local:")
            .unwrap_or(&owner.record.owner.project_id);
        let mut dir = home.join("engine");
        let name = format!("{}.sock", &hash[..hash.len().min(32)]);
        if dir.join(&name).as_os_str().len() >= 100 {
            dir = PathBuf::from(format!("/tmp/rudder-engine-{}", unsafe { libc::getuid() }));
        }
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let path = dir.join(name);
        if path.exists() {
            anyhow::ensure!(
                !attach_is_live(&path)?,
                "an engine control socket is already live"
            );
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path).context("bind engine control socket")?;
        let metadata = fs::symlink_metadata(&path)?;
        let identity = (metadata.dev(), metadata.ino());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let (tx, rx) = mpsc::sync_channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("rudder-engine-control".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if prepare_control_stream(&stream).is_err() {
                                continue;
                            }
                            let request = read_control(&stream).ok();
                            let Some(request) = request else {
                                continue;
                            };
                            let (response, response_rx) = mpsc::sync_channel(1);
                            if tx.try_send(ControlEvent { request, response }).is_err() {
                                continue;
                            }
                            if let Some(waker) = &waker {
                                waker();
                            }
                            if let Ok(value) = response_rx.recv_timeout(Duration::from_secs(3)) {
                                if let Ok(bytes) = serde_json::to_vec(&value) {
                                    let _ = stream.write_all(&bytes);
                                    let _ = stream.write_all(b"\n");
                                }
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            path,
            identity,
            rx,
            stopping: false,
            stop,
            thread: Some(thread),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The App's mutation thread alone accepts shutdown and answers state.
    pub(crate) fn poll(&mut self, owner: &mut EngineOwnership, app: &App) -> bool {
        for _ in 0..MAX_EVENTS_PER_FRAME {
            let Ok(event) = self.rx.try_recv() else {
                break;
            };
            let request = &event.request;
            let valid = request.authenticates(&owner.record);
            let response = if !valid {
                serde_json::json!({"ok":false,"error":"engine control identity or epoch mismatch"})
            } else if !matches!(request.op.as_str(), "status" | "shutdown") {
                serde_json::json!({"ok":false,"error":"unknown engine control operation"})
            } else {
                if request.op == "shutdown" {
                    self.stopping = true;
                    owner.record.state = "stopping".into();
                }
                let mut status = owner.record.public(true);
                status["agent_count"] = serde_json::json!(app.agents.len());
                status["live_ptys"] = serde_json::json!(app
                    .agents
                    .iter()
                    .filter(|run| {
                        run.terminal
                            .as_ref()
                            .and_then(TerminalPane::child_process_id)
                            .is_some()
                    })
                    .count());
                status["attached_views"] = serde_json::json!(app
                    .attach_clients
                    .iter()
                    .filter(|client| client.dashboard.is_some() || client.run_id.is_some())
                    .count());
                serde_json::json!({"ok":true,"status":status})
            };
            let _ = event.response.send(response);
        }
        self.stopping
    }
}

impl Drop for EngineControl {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // A retired owner may still hold the old listener after another epoch
        // replaced the pathname. Only unlink the endpoint this object bound.
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn read_control(stream: &UnixStream) -> Result<ControlRequest> {
    let mut line = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut reader = stream;
    let mut part = [0u8; 1024];
    while line.len() <= CONTROL_LIMIT {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "engine control request timed out");
        // A partial read must not restart the entire request's deadline.
        stream.set_read_timeout(Some(remaining))?;
        let capacity = part.len().min(CONTROL_LIMIT + 1 - line.len());
        let count = match reader.read(&mut part[..capacity]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if count == 0 { break; }
        let end = part[..count].iter().position(|byte| *byte == b'\n');
        line.extend_from_slice(&part[..end.map_or(count, |at| at + 1)]);
        if end.is_some() { break; }
    }
    anyhow::ensure!(
        line.len() <= CONTROL_LIMIT && line.last() == Some(&b'\n'),
        "invalid engine control request size"
    );
    Ok(serde_json::from_slice(&line)?)
}

fn request_control(record: &OwnerRecord, op: &str) -> Result<serde_json::Value> {
    let socket = record
        .control_socket
        .as_ref()
        .context("owner has no engine control socket")?;
    let mut stream =
        connect_local(socket.to_path_buf()).context("engine control socket is not answering")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let request = ControlRequest {
        format_version: ENGINE_FORMAT,
        op: op.into(),
        instance_id: record.instance_id.clone(),
        epoch: record.owner.epoch,
        nonce: record
            .control_nonce
            .clone()
            .context("owner has no control nonce")?,
    };
    stream.write_all(&serde_json::to_vec(&request)?)?;
    stream.write_all(b"\n")?;
    let mut bytes = Vec::new();
    BufReader::new(stream)
        .take((CONTROL_LIMIT + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= CONTROL_LIMIT,
        "oversized engine control response"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        value["ok"] == true,
        "{}",
        value["error"].as_str().unwrap_or("engine control refused")
    );
    let status = &value["status"];
    anyhow::ensure!(
        status["instance_id"] == record.instance_id
            && status["owner"]["epoch"] == record.owner.epoch,
        "engine control answered for a different owner epoch"
    );
    Ok(status.clone())
}

fn ownership_lock_held(repo: &Path) -> Result<bool> {
    let path = repo.join(".rudder/engine/owner.lock");
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            FileExt::unlock(&file)?;
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// Explicit unlock also covers errors before EngineOwnership is constructed.
/// A concurrent fork can temporarily inherit a close-on-exec descriptor; merely
/// closing the parent's copy would keep its flock held until that child execs.
struct CheckoutLock(File);
impl std::ops::Deref for CheckoutLock {
    type Target = File;
    fn deref(&self) -> &File { &self.0 }
}
impl Drop for CheckoutLock {
    fn drop(&mut self) {
        // Unclean failure leaves journal authority uncertain for reconciliation.
        // Never unlink the lock inode, which another owner may be waiting on.
        let _ = FileExt::unlock(&self.0);
    }
}

fn acquire_checkout_lock(repo: &Path) -> Result<CheckoutLock> {
    let directory = repo.join(".rudder/engine");
    fs::create_dir_all(&directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("owner.lock"))?;
    FileExt::try_lock_exclusive(&file).map_err(|error| anyhow::anyhow!(
        "another Rudder owner holds this checkout; attach to it or shut it down explicitly ({error})"
    ))?;
    Ok(CheckoutLock(file))
}

fn absent_status(repo: &Path) -> serde_json::Value {
    serde_json::json!({"format_version":ENGINE_FORMAT,"running":false,"repo":repo,
        "authority":"fabric","implementation":"legacy-native-extraction","state":"not-running",
        "pid":null,"owner":null,"instance_id":null,
        "attach_socket":repo.join(".rudder").join(ATTACH_SOCKET_NAME),"control_socket":null})
}

pub(crate) fn engine_status(repo: &Path, shutdown: bool) -> Result<serde_json::Value> {
    let held = ownership_lock_held(repo)?;
    let attach_live = attach_is_live(&repo.join(".rudder").join(ATTACH_SOCKET_NAME))?;
    // A stopped projection can be stale after a checkout move or interrupted
    // metadata write. It is not the authority and must not prevent recovery.
    if !held && !attach_live {
        return Ok(absent_status(repo));
    }
    let path = repo.join(".rudder/engine/owner.json");
    let record: Option<OwnerRecord> = match fs::read(path) {
        Ok(bytes) => {
            Some(serde_json::from_slice(&bytes).context("invalid engine owner projection")?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let Some(record) = record else {
        if held || attach_live {
            if shutdown {
                bail!("a legacy owner is live; close its own dashboard explicitly");
            }
            let mut value = absent_status(repo);
            value["running"] = true.into();
            value["authority"] = "legacy-native".into();
            value["state"] = "legacy-owner".into();
            return Ok(value);
        }
        return Ok(absent_status(repo));
    };
    anyhow::ensure!(
        record.format_version == ENGINE_FORMAT && record.repo == repo,
        "owner projection does not match this checkout"
    );
    if record.authority == "legacy-native" {
        anyhow::ensure!(
            !shutdown,
            "a legacy owner is live; close its own dashboard explicitly"
        );
        return Ok(record.public(true));
    }
    anyhow::ensure!(
        held,
        "engine attach endpoint is live without its ownership lock; refusing control"
    );
    request_control(&record, if shutdown { "shutdown" } else { "status" })
}

pub(crate) fn run_engine_command(args: &[String]) -> Result<()> {
    let mut operation = "serve";
    let mut repo = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "serve" | "status" | "shutdown" => operation = arg,
            "--repo" | "--cwd" => {
                repo = Some(PathBuf::from(
                    iter.next().context("missing engine checkout")?,
                ))
            }
            "--json" => {}
            "--backend" | "-b" | "--model" | "-m" => {
                iter.next().context("missing engine model selection")?;
            }
            "-h" | "--help" => {
                println!(
                    "rudder-native engine [serve|status|shutdown] [--repo <checkout>] [--json]"
                );
                return Ok(());
            }
            other => bail!("unknown engine argument: {other}"),
        }
    }
    let selected = repo.unwrap_or(env::current_dir()?);
    let repo = dashboard_root(&selected)
        .canonicalize()
        .context("engine checkout")?;
    if operation == "serve" {
        env::set_current_dir(&repo)?;
        let ownership = EngineOwnership::acquire(&repo, NativeAuthority::Fabric)?;
        run_app_loop(None, ownership)
    } else {
        println!(
            "{}",
            serde_json::to_string(&engine_status(&repo, operation == "shutdown")?)?
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "engine_host_tests.rs"]
mod tests;

/// Launch-time profile selection only. Real authorization still uses the held
/// owner lock and journal fence; this projection cannot grant an agent power.
pub(crate) fn current_process_is_fabric_owner() -> bool {
    let Ok(repo) = std::env::current_dir().and_then(|path| path.canonicalize()) else { return false; };
    let path = repo.join(".rudder/engine/owner.json");
    let Ok(file) = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path) else { return false; };
    if !file.metadata().is_ok_and(|meta| meta.is_file() && meta.len() <= 16 * 1024) { return false; }
    let Ok(record) = serde_json::from_reader::<_,OwnerRecord>(file) else { return false; };
    record.format_version == ENGINE_FORMAT && record.authority == "fabric" && record.pid == std::process::id() && record.repo == repo
}

#[cfg(target_os="linux")]
#[path="engine_host_ingress_acquisition.rs"]
mod ingress_acquisition;
#[cfg(target_os="linux")]
pub(crate) use ingress_acquisition::RetainedManagedAcquisition;

#[cfg(target_os="linux")]
#[path="engine_host_fresh.rs"]
mod fresh;
#[cfg(target_os="linux")]
pub(crate) use fresh::{FreshLocalNamespace,FreshNamespaceCustody};
