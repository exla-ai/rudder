//! Existing-user authority migration. This module never schedules or resumes.
//! The same OS owner lock as engine_host is retained for every effect. A receipt
//! is recovery evidence, not a live fence. Uncooperative old clients still need
//! confirmed shutdown and disabled launchers/credentials outside this module.
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use rudder_engine::journal::{Journal, OwnerFence};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyRun {
    pub run_id: String,
    /// None means absent at preparation; Some binds an existing legacy adapter run.
    pub expected_epoch: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationRequest {
    pub format_version: u32,
    pub migration_id: String,
    pub repository: PathBuf,
    pub previous_authority: String,
    pub previous_owner: OwnerFence,
    pub next_owner_id: String,
    pub runs: Vec<LegacyRun>,
    pub recovery_bundle: PathBuf,
    pub recovery_manifest_sha256: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Exported,
    Importing,
    Imported,
    Transferring,
    Committed,
    RollbackPausing,
    RollbackPaused,
    RolledBack,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationReceipt {
    pub format_version: u32,
    pub request: MigrationRequest,
    pub phase: Phase,
    pub imported: usize,
    pub owner: OwnerFence,
    pub run_epochs: BTreeMap<String, u64>,
    pub rollback_checkpoint: Option<String>,
    pub rollback_bundle: Option<PathBuf>,
    pub rollback_manifest_sha256: Option<String>,
}
/// Required explicit handover assertions, supplied by the trusted host after
/// checking its actual shutdown/drain/fencing producers. No default/serde.
/// These are NOT proof that an incompatible executable is physically fenced.
pub struct Quiescence {
    pub scheduler_shutdown_confirmed: bool,
    pub effects_reconciled: bool,
    pub launchers_disabled: bool,
    pub external_credentials_fenced: bool,
}
impl Quiescence {
    fn check(&self) -> Result<()> {
        ensure!(self.scheduler_shutdown_confirmed && self.effects_reconciled && self.launchers_disabled && self.external_credentials_fenced,
            "migration requires confirmed shutdown, reconciled effects, disabled launchers and fenced credentials");
        Ok(())
    }
}

pub struct MigrationSession {
    repository: PathBuf,
    _owner_lock: File,
    receipt_path: PathBuf,
}
impl MigrationSession {
    /// Run off the App loop. Never call while engine_host already owns its lock.
    /// The caller releases/stops its actual owner first; this method never kills it.
    pub fn open(repository: &Path, quiescence: &Quiescence) -> Result<Self> {
        quiescence.check()?;
        let repository = fs::canonicalize(repository)?;
        reject_live_legacy(&repository)?;
        let engine = repository.join(".rudder/engine");
        for directory in [repository.join(".rudder"), engine.clone()] {
            match fs::symlink_metadata(&directory) {
                Ok(stat) => ensure!(
                    stat.is_dir() && !stat.is_symlink(),
                    "migration directory cannot be linked"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&directory)?,
                Err(e) => return Err(e.into()),
            }
        }
        let lock_path = engine.join("owner.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock_path)?;
        lock.try_lock_exclusive()
            .context("project owner is live; migration refused")?;
        reject_live_legacy(&repository)?;
        Ok(Self {
            receipt_path: engine.join("v4-migration.json"),
            repository,
            _owner_lock: lock,
        })
    }
    /// Idempotency is the full immutable request, not just its migration ID.
    pub fn prepare(
        &self,
        journal: &Journal,
        request: MigrationRequest,
    ) -> Result<MigrationReceipt> {
        ensure!(
            request.format_version == 1
                && !request.migration_id.is_empty()
                && request.repository == self.repository
                && ["legacy-native", "legacy-ts"].contains(&request.previous_authority.as_str())
                && !request.next_owner_id.is_empty()
                && request.next_owner_id != request.previous_owner.owner_id,
            "invalid migration request"
        );
        ensure!(
            request
                .runs
                .iter()
                .all(|r| !r.run_id.is_empty() && r.expected_epoch != Some(0))
                && request
                    .runs
                    .iter()
                    .map(|r| &r.run_id)
                    .collect::<BTreeSet<_>>()
                    .len()
                    == request.runs.len(),
            "invalid migration run set"
        );
        if let Some(existing) = self.read()? {
            ensure!(
                existing.request == request,
                "migration idempotency conflict"
            );
            return Ok(existing);
        }
        let state = journal.project_state(&request.previous_owner.project_id)?;
        ensure!(
            state.owner_active && state.owner == request.previous_owner,
            "stale migration owner epoch"
        );
        let epochs = epochs(journal, &request.previous_owner)?;
        // Full inventory, including completed/cancelled runs, must be preserved.
        for (id, epoch) in &epochs {
            ensure!(
                request
                    .runs
                    .iter()
                    .any(|r| &r.run_id == id && r.expected_epoch == Some(*epoch)),
                "migration omits an existing run epoch"
            );
        }
        for run in &request.runs {
            ensure!(
                epochs.get(&run.run_id).copied() == run.expected_epoch,
                "stale migration run epoch"
            );
        }
        verify_bundle(
            &request.recovery_bundle,
            &request.recovery_manifest_sha256,
            &request.previous_owner.project_id,
            &request.migration_id,
        )?;
        verify_legacy_inventory(&request)?;
        let receipt = MigrationReceipt {
            format_version: 1,
            owner: request.previous_owner.clone(),
            request,
            phase: Phase::Prepared,
            imported: 0,
            run_epochs: epochs,
            rollback_checkpoint: None,
            rollback_bundle: None,
            rollback_manifest_sha256: None,
        };
        self.save(&receipt)?;
        Ok(receipt)
    }
    pub fn read(&self) -> Result<Option<MigrationReceipt>> {
        match fs::symlink_metadata(&self.receipt_path) {
            Ok(stat) => ensure!(
                stat.is_file() && stat.len() <= 8 * 1024 * 1024,
                "invalid migration receipt file"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let bytes = match fs::read(&self.receipt_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let receipt: MigrationReceipt = serde_json::from_slice(&bytes)?;
        ensure!(
            receipt.format_version == 1
                && receipt.request.format_version == 1
                && receipt.request.repository == self.repository
                && receipt.owner.project_id == receipt.request.previous_owner.project_id
                && receipt.imported <= receipt.request.runs.len(),
            "invalid migration receipt"
        );
        Ok(Some(receipt))
    }
    /// One committed phase or one run import per call. Intents are persisted
    /// BEFORE journal writes. A crash after SQLite commit reconciles the exact
    /// owner/run epoch; it never observes a runtime or starts a model turn.
    pub fn advance(
        &self,
        journal: &mut Journal,
        migration_id: &str,
        now_ms: i64,
    ) -> Result<MigrationReceipt> {
        reject_live_legacy(&self.repository)?;
        let mut receipt = self.required(migration_id)?;
        match receipt.phase {
            Phase::Prepared => {
                verify_bundle(
                    &receipt.request.recovery_bundle,
                    &receipt.request.recovery_manifest_sha256,
                    &receipt.owner.project_id,
                    migration_id,
                )?;
                self.assert_owner(journal, &receipt.owner)?;
                validate_epochs(journal, &receipt.owner, &receipt.run_epochs)?;
                receipt.phase = Phase::Exported;
            }
            Phase::Exported => {
                self.assert_owner(journal, &receipt.owner)?;
                validate_epochs(journal, &receipt.owner, &receipt.run_epochs)?;
                receipt.phase = Phase::Importing;
            }
            Phase::Importing => {
                self.assert_owner(journal, &receipt.owner)?;
                if receipt.imported < receipt.request.runs.len() {
                    let run = &receipt.request.runs[receipt.imported];
                    let current = epochs(journal, &receipt.owner)?.get(&run.run_id).copied();
                    let mut expected = receipt.run_epochs.clone();
                    if run.expected_epoch.is_none() && current == Some(1) {
                        expected.insert(run.run_id.clone(), 1);
                    }
                    validate_epochs(journal, &receipt.owner, &expected)?;
                    match (run.expected_epoch, current) {
                        (Some(expected), Some(actual)) => {
                            ensure!(expected == actual, "stale imported run epoch")
                        }
                        (None, None) => {
                            journal.register_run(&receipt.owner, &run.run_id, now_ms)?;
                        }
                        // Only the current durable Importing slot can reconcile a
                        // registration committed before its file receipt.
                        (None, Some(1)) => {}
                        _ => bail!("partial import conflicts with the recorded run epoch"),
                    }
                    receipt.imported += 1;
                } else {
                    validate_epochs(journal, &receipt.owner, &receipt.run_epochs)?;
                    receipt.phase = Phase::Imported;
                }
                receipt.run_epochs = epochs(journal, &receipt.owner)?;
            }
            Phase::Imported => {
                self.assert_owner(journal, &receipt.owner)?;
                validate_epochs(journal, &receipt.owner, &receipt.run_epochs)?;
                receipt.phase = Phase::Transferring;
            }
            Phase::Transferring => {
                let state = journal.project_state(&receipt.owner.project_id)?;
                let next = OwnerFence {
                    project_id: receipt.owner.project_id.clone(),
                    owner_id: receipt.request.next_owner_id.clone(),
                    epoch: receipt
                        .request
                        .previous_owner
                        .epoch
                        .checked_add(1)
                        .context("owner epoch exhausted")?,
                };
                ensure!(state.owner_active, "migration owner is no longer active");
                if state.owner == receipt.request.previous_owner {
                    validate_epochs(journal, &state.owner, &receipt.run_epochs)?;
                    receipt.owner = journal.transfer_owner(&state.owner, &next.owner_id, now_ms)?;
                } else {
                    ensure!(
                        state.owner == next,
                        "migration ownership effect requires reconciliation"
                    );
                    let expected = increment_epochs(&receipt.run_epochs)?;
                    validate_epochs(journal, &next, &expected)?;
                    receipt.owner = next;
                }
                receipt.run_epochs = epochs(journal, &receipt.owner)?;
                receipt.phase = Phase::Committed;
            }
            Phase::Committed => {
                self.assert_owner(journal, &receipt.owner)?;
                validate_epochs(journal, &receipt.owner, &receipt.run_epochs)?;
            }
            _ => bail!("rollback is in progress; migration cannot resume"),
        }
        self.save(&receipt)?;
        Ok(receipt)
    }
    /// Rollback first records intent and releases the precise current owner.
    /// The next call reconciles a release interrupted after SQLite commit.
    pub fn pause_for_rollback(
        &self,
        journal: &mut Journal,
        migration_id: &str,
        now_ms: i64,
    ) -> Result<MigrationReceipt> {
        reject_live_legacy(&self.repository)?;
        let mut receipt = self.required(migration_id)?;
        if receipt.phase == Phase::RolledBack || receipt.phase == Phase::RollbackPaused {
            self.assert_paused(journal, &receipt)?;
            return Ok(receipt);
        }
        if receipt.phase != Phase::RollbackPausing {
            // Settle an uncertain transfer rather than release a stale fence.
            if receipt.phase == Phase::Transferring {
                receipt = self.advance(journal, migration_id, now_ms)?;
            }
            self.assert_owner(journal, &receipt.owner)?;
            // Reconcile ONLY the current in-flight registration slot. All other
            // epoch/inventory changes require an explicit current rollback fence.
            let mut expected = receipt.run_epochs.clone();
            if receipt.phase == Phase::Importing {
                if let Some(run) = receipt.request.runs.get(receipt.imported) {
                    if run.expected_epoch.is_none()
                        && epochs(journal, &receipt.owner)?.get(&run.run_id) == Some(&1)
                    {
                        expected.insert(run.run_id.clone(), 1);
                    }
                }
            }
            validate_epochs(journal, &receipt.owner, &expected)?;
            receipt.run_epochs = expected;
            receipt.phase = Phase::RollbackPausing;
            self.save(&receipt)?;
        }
        // Bind a later exported archive to this exact durable paused checkpoint.
        // This opaque receipt label is recovery data, never launch authority.
        if receipt.rollback_checkpoint.is_none() {
            receipt.rollback_checkpoint = Some(uuid::Uuid::new_v4().to_string());
            self.save(&receipt)?;
        }
        let state = journal.project_state(&receipt.owner.project_id)?;
        if state.owner_active && state.owner == receipt.owner {
            validate_epochs(journal, &receipt.owner, &receipt.run_epochs)?;
            journal.release_owner(&receipt.owner, now_ms)?;
        } else {
            ensure!(
                !state.owner_active
                    && state.owner.owner_id == receipt.owner.owner_id
                    && state.owner.epoch
                        == receipt
                            .owner
                            .epoch
                            .checked_add(1)
                            .context("owner epoch exhausted")?,
                "rollback release requires reconciliation"
            );
            for (run, epoch) in increment_epochs(&receipt.run_epochs)? {
                ensure!(
                    journal.run_epoch(&receipt.owner.project_id, &run)? == epoch,
                    "stale rollback run epoch"
                );
            }
        }
        receipt.owner = journal.project_state(&receipt.owner.project_id)?.owner;
        receipt.run_epochs = receipt
            .run_epochs
            .keys()
            .map(|run| {
                Ok((
                    run.clone(),
                    journal.run_epoch(&receipt.owner.project_id, run)?,
                ))
            })
            .collect::<Result<_>>()?;
        receipt.phase = Phase::RollbackPaused;
        self.save(&receipt)?;
        Ok(receipt)
    }
    /// Explicit rollback after normal v4 work changed run or owner epochs.
    /// The trusted host supplies its fresh complete inventory under this same
    /// retained OS lock. Stale saved receipts alone cannot stop a newer owner.
    pub fn pause_for_rollback_at(
        &self,
        journal: &mut Journal,
        migration_id: &str,
        expected_owner: &OwnerFence,
        expected_runs: &BTreeMap<String, u64>,
        now_ms: i64,
    ) -> Result<MigrationReceipt> {
        reject_live_legacy(&self.repository)?;
        let mut receipt = self.required(migration_id)?;
        ensure!(
            receipt.phase == Phase::Committed,
            "explicit rollback fence requires a committed migration"
        );
        ensure!(
            expected_owner.project_id == receipt.owner.project_id
                && expected_owner.epoch >= receipt.owner.epoch,
            "rollback fence belongs to another or older owner"
        );
        self.assert_owner(journal, expected_owner)?;
        validate_epochs(journal, expected_owner, expected_runs)?;
        receipt.owner = expected_owner.clone();
        receipt.run_epochs = expected_runs.clone();
        receipt.phase = Phase::RollbackPausing;
        self.save(&receipt)?;
        self.pause_for_rollback(journal, migration_id, now_ms)
    }
    /// Caller seals/exports v4 deltas AFTER pause, then invokes this. Both the
    /// original and rollback archives remain retained. No source is overwritten,
    /// no config mode is switched and no legacy scheduler is automatically started.
    pub fn finish_rollback(
        &self,
        journal: &Journal,
        migration_id: &str,
        bundle: &Path,
        sha256: &str,
    ) -> Result<MigrationReceipt> {
        let mut receipt = self.required(migration_id)?;
        ensure!(
            matches!(receipt.phase, Phase::RollbackPaused | Phase::RolledBack),
            "pause the new owner before exporting rollback deltas"
        );
        self.assert_paused(journal, &receipt)?;
        reject_live_legacy(&self.repository)?;
        ensure!(
            bundle != receipt.request.recovery_bundle,
            "rollback requires a separate post-pause delta export"
        );
        verify_bundle(bundle, sha256, &receipt.owner.project_id, migration_id)?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(bundle.join("manifest.json"))?)?;
        ensure!(
            receipt.rollback_checkpoint.is_some()
                && manifest["rollback_checkpoint"].as_str()
                    == receipt.rollback_checkpoint.as_deref(),
            "rollback export is not bound to this paused checkpoint"
        );
        if receipt.phase == Phase::RolledBack {
            ensure!(
                receipt.rollback_bundle.as_deref() == Some(bundle)
                    && receipt.rollback_manifest_sha256.as_deref() == Some(sha256),
                "rollback idempotency conflict"
            );
            return Ok(receipt);
        }
        receipt.rollback_bundle = Some(bundle.to_path_buf());
        receipt.rollback_manifest_sha256 = Some(sha256.into());
        receipt.phase = Phase::RolledBack;
        self.save(&receipt)?;
        Ok(receipt)
    }
    fn assert_paused(&self, journal: &Journal, receipt: &MigrationReceipt) -> Result<()> {
        let state = journal.project_state(&receipt.owner.project_id)?;
        ensure!(
            !state.owner_active && state.owner == receipt.owner,
            "rollback owner changed"
        );
        for (run, epoch) in &receipt.run_epochs {
            ensure!(
                journal.run_epoch(&receipt.owner.project_id, run)? == *epoch,
                "stale rollback run epoch"
            );
        }
        Ok(())
    }
    fn assert_owner(&self, journal: &Journal, owner: &OwnerFence) -> Result<()> {
        let state = journal.project_state(&owner.project_id)?;
        ensure!(
            state.owner_active && state.owner == *owner,
            "stale migration owner epoch"
        );
        Ok(())
    }
    fn required(&self, migration_id: &str) -> Result<MigrationReceipt> {
        let receipt = self.read()?.context("migration is not prepared")?;
        ensure!(
            receipt.request.migration_id == migration_id,
            "migration idempotency conflict"
        );
        Ok(receipt)
    }
    fn save(&self, receipt: &MigrationReceipt) -> Result<()> {
        let temp = self
            .receipt_path
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let result = (|| {
            file.write_all(&serde_json::to_vec_pretty(receipt)?)?;
            file.sync_all()?;
            fs::rename(&temp, &self.receipt_path)?;
            File::open(self.receipt_path.parent().unwrap())?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}
fn epochs(journal: &Journal, owner: &OwnerFence) -> Result<BTreeMap<String, u64>> {
    Ok(journal
        .owned_runs(owner)?
        .into_iter()
        .map(|run| (run.run_id, run.run_epoch))
        .collect())
}
fn validate_epochs(
    journal: &Journal,
    owner: &OwnerFence,
    expected: &BTreeMap<String, u64>,
) -> Result<()> {
    ensure!(
        epochs(journal, owner)? == *expected,
        "stale migration run inventory"
    );
    Ok(())
}
fn increment_epochs(epochs: &BTreeMap<String, u64>) -> Result<BTreeMap<String, u64>> {
    epochs
        .iter()
        .map(|(id, epoch)| {
            Ok((
                id.clone(),
                epoch.checked_add(1).context("run epoch exhausted")?,
            ))
        })
        .collect()
}
fn live(pid: u64) -> bool {
    if pid == 0 || pid > i32::MAX as u64 {
        return true;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
fn metadata(path: &Path) -> Result<Option<serde_json::Value>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(stat) => ensure!(
            stat.is_file() && stat.len() <= 8 * 1024 * 1024,
            "unsupported legacy metadata; migration refused"
        ),
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}
fn check_pid(value: &serde_json::Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let pid = value
        .as_u64()
        .context("invalid legacy PID; cannot establish shutdown")?;
    ensure!(
        !live(pid),
        "known live legacy owner PID {pid}; stop its owner explicitly before migration"
    );
    Ok(())
}
pub fn reject_live_legacy(repo: &Path) -> Result<()> {
    let runs = repo.join(".rudder/runs");
    if let Ok(stat) = fs::symlink_metadata(&runs) {
        ensure!(
            stat.is_dir() && !stat.is_symlink(),
            "symlinked legacy store cannot establish shutdown"
        );
    }
    match fs::read_dir(runs) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                ensure!(
                    !entry.file_type()?.is_symlink(),
                    "symlinked legacy run cannot establish shutdown"
                );
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if let Some(value) = metadata(&entry.path().join("run.json"))? {
                    ensure!(
                        value.is_object(),
                        "unsupported legacy run record; cannot establish shutdown"
                    );
                    if let Some(process) = value.get("process") {
                        ensure!(
                            process.is_null() || process.is_object(),
                            "invalid legacy process metadata"
                        );
                    }
                    for name in ["pid", "controllerPid", "backendPid"] {
                        if let Some(pid) = value.get("process").and_then(|p| p.get(name)) {
                            check_pid(pid)?;
                        }
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    for name in [".rudder/daemon.pid", ".rudder/engine/owner.json"] {
        if let Some(value) = metadata(&repo.join(name))? {
            // Old daemon.pid files sometimes contained a bare JSON integer.
            if value.is_number() {
                check_pid(&value)?;
                continue;
            }
            ensure!(
                value.is_object(),
                "unsupported legacy owner metadata; cannot establish shutdown"
            );
            if value.get("controlMode").and_then(|v| v.as_str()) != Some("projector") {
                if let Some(pid) = value.get("pid") {
                    check_pid(pid)?;
                }
            }
        }
    }
    Ok(())
}

/// Read-only, daemonless checksum validation. No CAS publication or source
/// authority is implied by a bundle. Keep its complete directory retained.
fn verify_bundle(bundle: &Path, hash: &str, project: &str, migration: &str) -> Result<()> {
    ensure!(
        bundle.is_absolute()
            && hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid recovery checkpoint"
    );
    let manifest_path = bundle.join("manifest.json");
    ensure!(
        fs::symlink_metadata(&manifest_path)?.is_file(),
        "recovery manifest must be an ordinary file"
    );
    let bytes = fs::read(&manifest_path)?;
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == hash,
        "recovery manifest checksum mismatch"
    );
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest["format_version"] == 1
            && manifest["kind"] == "v4_legacy_recovery"
            && manifest["jj_version"] == "0.40.0"
            && manifest["project_id"] == project
            && manifest["migration_id"] == migration,
        "recovery checkpoint identity mismatch"
    );
    let roots = manifest["roots"]
        .as_array()
        .context("missing recovery roots")?;
    let mut root_names = BTreeSet::new();
    for root in roots {
        let name = root["name"]
            .as_str()
            .context("missing recovery root name")?;
        ensure!(
            safe_relative(name)
                && !name.is_empty()
                && !name.contains('/')
                && root_names.insert(name),
            "invalid or duplicate recovery root"
        );
    }
    ensure!(!roots.is_empty(), "empty recovery roots");
    let entries = manifest["entries"]
        .as_array()
        .context("missing recovery entries")?;
    ensure!(!entries.is_empty(), "empty recovery entries");
    let mut seen = BTreeMap::new();
    for entry in entries {
        let root = entry["root"].as_str().context("missing entry root")?;
        let relative = entry["path"].as_str().context("missing entry path")?;
        let kind = entry["type"].as_str().context("missing entry type")?;
        let mode = entry["mode"].as_u64().context("missing entry mode")?;
        ensure!(
            root_names.contains(root)
                && safe_relative(relative)
                && mode <= 0o777
                && ["directory", "file", "symlink"].contains(&kind),
            "invalid recovery entry"
        );
        if !relative.is_empty() {
            let parent = relative.rsplit_once('/').map_or("", |(parent, _)| parent);
            ensure!(
                seen.get(&(root, parent)) == Some(&"directory"),
                "recovery entry traverses a missing directory or symlink"
            );
        }
        ensure!(
            seen.insert((root, relative), kind).is_none(),
            "duplicate recovery entry"
        );
        if kind == "directory" {
            ensure!(entry["object"].is_null(), "unexpected directory object");
            continue;
        }
        let reference = &entry["object"];
        let object_hash = reference["digest"]
            .as_str()
            .context("missing recovery digest")?;
        ensure!(
            reference["kind"] == "blob"
                && reference["algorithm"] == "sha256"
                && object_hash.len() == 64
                && object_hash
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid recovery blob reference"
        );
        let object_path = bundle.join("objects").join(object_hash);
        ensure!(
            fs::symlink_metadata(&object_path)?.is_file(),
            "recovery blob must be an ordinary file"
        );
        let bytes = fs::read(&object_path)?;
        ensure!(
            reference["size_bytes"].as_u64() == Some(bytes.len() as u64)
                && hex::encode(Sha256::digest(bytes)) == object_hash,
            "recovery blob checksum mismatch"
        );
        File::open(object_path)?.sync_all()?;
    }
    ensure!(
        root_names
            .iter()
            .all(|root| seen.contains_key(&(*root, ""))),
        "missing recovery root entry"
    );
    File::open(manifest_path)?.sync_all()?;
    File::open(bundle.join("objects"))?.sync_all()?;
    File::open(bundle)?.sync_all()?;
    Ok(())
}

/// Wire into native legacy/default startup before acquiring an owner or changing
/// authority configuration. Completed rollback also requires an explicit adapter
/// handoff, so merely opening an old dashboard never resumes migrated work.
pub fn refuse_implicit_startup(repository: &Path) -> Result<()> {
    match fs::symlink_metadata(repository.join(".rudder/engine/v4-migration.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(_) => bail!(
            "project migration requires explicit recovery; implicit scheduler startup refused"
        ),
    }
}

fn safe_relative(path: &str) -> bool {
    path.is_empty()
        || (!path.contains('\\')
            && !path.contains('\0')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}

fn verify_legacy_inventory(request: &MigrationRequest) -> Result<()> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(request.recovery_bundle.join("manifest.json"))?)?;
    let roots = manifest["roots"]
        .as_array()
        .context("missing legacy recovery roots")?;
    for required in ["integration-source", "git-history", "jj-history"] {
        ensure!(
            roots.iter().any(|root| root["name"] == required),
            "legacy recovery is missing integration source or history"
        );
    }
    let entries = manifest["entries"]
        .as_array()
        .context("missing legacy recovery entries")?;
    let mut records = BTreeSet::new();
    for entry in entries {
        if entry["root"] != "legacy-runs"
            || entry["type"] != "file"
            || !entry["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("/run.json"))
        {
            continue;
        }
        let hash = entry["object"]["digest"]
            .as_str()
            .context("missing legacy record object")?;
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(
            request.recovery_bundle.join("objects").join(hash),
        )?)?;
        let id = raw["id"]
            .as_str()
            .context("legacy record has no run identity")?;
        ensure!(
            raw["status"].is_string() && records.insert(id.to_string()),
            "invalid or duplicate legacy record"
        );
        if raw["session"]["nativeSessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        {
            ensure!(
                roots
                    .iter()
                    .any(|root| root["role"] == "sessions" && root["run_id"] == id),
                "legacy session transcript is missing from recovery"
            );
        }
    }
    ensure!(
        records == request.runs.iter().map(|run| run.run_id.clone()).collect(),
        "legacy recovery run inventory differs from migration request"
    );
    Ok(())
}
