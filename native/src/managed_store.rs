//! Private managed-project recovery state. All filesystem APIs here are
//! off-App; only location cloning and bounded projection admission are pure
//! App operations. A prepared floor/receipt never authorizes a new execution.
#[path = "managed_source_records.rs"]
mod source_records;
pub(crate) use source_records::SourceCheckpointInputs;
use crate::managed_launch::{ManagedLaunchFence, ManagedRunSource, WorkspaceAssignment};
use anyhow::{ensure, Context, Result};
use rudder_engine::{
    journal::{Journal, ManagedProjectNamespace, ManagedProjectPrepare, ManagedProjectReceipt},
    protocol::canonical_json,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, SyncSender},
        OnceLock,
    },
};

const RECORD_LIMIT: usize = 64 * 1024;
const PROJECTION_LIMIT: usize = 512 * 1024;
const MAX_RUNS: usize = 4096;

fn private_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "managed state directory is not private/owned"
    );
    Ok(())
}
fn create_directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            File::open(path)?.sync_all()?;
            File::open(path.parent().context("managed state parent absent")?)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    private_directory(path)
}
fn read_bytes(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.nlink() == 1
            && meta.mode() & 0o077 == 0
            && meta.len() <= limit as u64,
        "unsafe managed state record"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "managed state record exceeds bound");
    Ok(Some(bytes))
}
fn read_record<T: DeserializeOwned + Serialize>(path: &Path) -> Result<Option<T>> {
    let Some(bytes) = read_bytes(path, RECORD_LIMIT)? else {
        return Ok(None);
    };
    let value = serde_json::from_slice::<T>(&bytes)?;
    ensure!(
        canonical_json(&value)? == bytes,
        "noncanonical managed state record"
    );
    Ok(Some(value))
}
/// Old shared metadata may legitimately be0644. Inspection and preservation
/// use this same bounded owned/no-follow policy; new copies are always0600.
fn read_experimental_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.nlink() == 1
            && meta.len() <= RECORD_LIMIT as u64,
        "experimental metadata exceeds owned regular-file bound"
    );
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= RECORD_LIMIT,
        "experimental metadata exceeds bound"
    );
    Ok(Some(bytes))
}
fn immutable<T: DeserializeOwned + Serialize + PartialEq>(path: &Path, value: &T) -> Result<()> {
    let bytes = canonical_json(value)?;
    ensure!(
        bytes.len() <= RECORD_LIMIT,
        "managed evidence exceeds bound"
    );
    private_directory(path.parent().context("managed evidence directory absent")?)?;
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                read_record::<T>(path)?.as_ref() == Some(value),
                "immutable managed evidence differs; explicit recovery required"
            );
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?
                .sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn frozen_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= RECORD_LIMIT,
        "experimental recovery record exceeds bound"
    );
    private_directory(path.parent().context("recovery record directory absent")?)?;
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                read_bytes(path, RECORD_LIMIT)?.as_deref() == Some(bytes),
                "experimental recovery snapshot differs"
            );
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?
                .sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Off-App identity establishment; checks actual canonical home/repo/database.
pub(crate) fn namespace(
    home: &Path,
    repo: &Path,
    project: &str,
    journal: &Journal,
) -> Result<ManagedProjectNamespace> {
    let home = home.canonicalize()?;
    let repository = repo.canonicalize()?;
    ensure!(
        rudder_engine::paths::local_project_id(&repository)? == project,
        "managed project differs from explicit repository identity"
    );
    let database = journal.database_path()?.canonicalize()?;
    ensure!(
        database == home.join("fabric/journal.sqlite") && database == journal.database_path()?,
        "managed journal differs from explicit canonical home"
    );
    let namespace = ManagedProjectNamespace {
        format_version: 1,
        project_id: project.into(),
        directory: ManagedProjectNamespace::directory_for(&home, project),
        home,
        repository,
        journal: database,
    };
    namespace.validate()?;
    private_directory(&namespace.home.join("fabric"))?;
    Ok(namespace)
}
fn validate_namespace(binding: &ManagedProjectNamespace, journal: &Journal) -> Result<()> {
    ensure!(
        namespace(
            &binding.home,
            &binding.repository,
            &binding.project_id,
            journal
        )? == *binding,
        "managed namespace identity changed"
    );
    Ok(())
}
/// A present namespace, even an incomplete/malformed one, blocks new legacy
/// startup. This lookup never creates, repairs or imports state.
pub(crate) fn has_namespace_evidence(home: &Path, project: &str) -> Result<bool> {
    let path = ManagedProjectNamespace::directory_for(home, project);
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
pub(crate) fn open_for_project(home: &Path, project: &str) -> Result<Option<ManagedStore>> {
    let path = home.join("fabric/journal.sqlite");
    if !path.try_exists()? {
        ensure!(
            !has_namespace_evidence(home, project)?,
            "managed namespace has no journal; recovery required"
        );
        return Ok(None);
    }
    let journal = Journal::open(&path)?;
    match journal.managed_project(project)? {
        Some(receipt) => Ok(Some(ManagedStore::open(
            &journal,
            &receipt.request.namespace,
        )?)),
        None => {
            ensure!(
                !has_namespace_evidence(home, project)?,
                "managed namespace has no writer floor; recovery required"
            );
            Ok(None)
        }
    }
}
pub(crate) fn run_directory(home: &Path, project: &str, run: &str) -> PathBuf {
    ManagedProjectNamespace::directory_for(home, project)
        .join("runs")
        .join(digest(run))
}
pub(crate) fn persist_prepare_intent(
    request: &ManagedProjectPrepare,
    journal: &Journal,
) -> Result<()> {
    request.validate()?;
    validate_namespace(&request.namespace, journal)?;
    let binding = &request.namespace;
    create_directory(&binding.home.join("fabric/managed-projects"))?;
    create_directory(&binding.directory)?;
    File::open(&binding.home)?.sync_all()?;
    immutable(&binding.directory.join("project.json"), request)
}
pub(crate) fn read_prepare_intent(
    binding: &ManagedProjectNamespace,
    journal: &Journal,
) -> Result<ManagedProjectPrepare> {
    validate_namespace(binding, journal)?;
    private_directory(&binding.home.join("fabric/managed-projects"))?;
    private_directory(&binding.directory)?;
    let request: ManagedProjectPrepare = read_record(&binding.directory.join("project.json"))?
        .context("managed prepare intent missing; explicit recovery required")?;
    request.validate()?;
    ensure!(
        request.namespace == *binding,
        "managed prepare intent identity differs"
    );
    Ok(request)
}
pub(crate) fn persist_prepared_receipt(
    receipt: &ManagedProjectReceipt,
    journal: &Journal,
) -> Result<ManagedStore> {
    receipt.validate()?;
    validate_namespace(&receipt.request.namespace, journal)?;
    ensure!(
        journal
            .managed_project(&receipt.request.namespace.project_id)?
            .as_ref()
            == Some(receipt),
        "prepared managed receipt is not journal evidence"
    );
    immutable(
        &receipt.request.namespace.directory.join("prepared.json"),
        receipt,
    )?;
    ManagedStore::open(journal, &receipt.request.namespace)
}
pub(crate) fn prepare_execution_parent(store: &ManagedStore) -> Result<()> {
    private_directory(&store.binding().directory)?;
    create_directory(&store.binding().directory.join("preparations"))
}

/// Explicit bounded recovery plan. Hashes select exact experimental metadata;
/// this is never a request to resume/import an old workspace or mint a grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExperimentalRecoveryPlan {
    format_version: u32,
    binding: ManagedProjectNamespace,
    entries: Vec<ExperimentalRecoveryEntry>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExperimentalRecoveryEntry {
    run_id: String,
    record_sha256: Option<String>,
    reservation_sha256: Option<String>,
    assignment: Option<(String, String)>,
}
fn byte_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
impl ExperimentalRecoveryPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    fn validate(&self, binding: &ManagedProjectNamespace) -> Result<()> {
        ensure!(
            self.format_version == 1
                && self.binding == *binding
                && self.entries.len() <= 128
                && canonical_json(self)?.len() <= RECORD_LIMIT,
            "invalid or excessive experimental recovery plan"
        );
        let mut last = None;
        for entry in &self.entries {
            ensure!(
                !entry.run_id.is_empty()
                    && entry.run_id.len() <= 256
                    && !entry.run_id.contains(['/', '\\'])
                    && !entry.run_id.chars().any(char::is_control)
                    && !matches!(entry.run_id.as_str(), "." | "..")
                    && last.is_none_or(|last: &str| last < entry.run_id.as_str()),
                "invalid/duplicate experimental run identity"
            );
            last = Some(&entry.run_id);
            let valid_hash = |hash: &str| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            };
            for hash in [&entry.record_sha256, &entry.reservation_sha256]
                .into_iter()
                .flatten()
            {
                ensure!(valid_hash(hash), "invalid recovery digest");
            }
            if let Some((name, hash)) = &entry.assignment {
                ensure!(
                    name.strip_suffix(".json").is_some_and(valid_hash) && valid_hash(hash),
                    "invalid experimental assignment selector"
                );
            }
        }
        Ok(())
    }
    fn directory(&self) -> Result<PathBuf> {
        Ok(self
            .binding
            .directory
            .join("experimental-recovery")
            .join(byte_digest(&canonical_json(self)?)))
    }
}
/// Off-App, called by the actual lock holder. Reads metadata only, never
/// provider profiles, runtime output, sessions or transcripts.
pub(crate) fn inspect_experimental(
    binding: &ManagedProjectNamespace,
) -> Result<ExperimentalRecoveryPlan> {
    binding.validate()?;
    let root = binding.repository.join(".rudder/runs");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if entries.is_some() {
        ensure!(
            fs::symlink_metadata(&root)?.is_dir(),
            "symlinked legacy run store cannot be recovered"
        );
    }
    let mut plan = ExperimentalRecoveryPlan {
        format_version: 1,
        binding: binding.clone(),
        entries: Vec::new(),
    };
    let mut total = 0usize;
    for (index, entry) in entries.into_iter().flatten().enumerate() {
        ensure!(index < MAX_RUNS, "experimental recovery scan exceeds bound");
        let entry = entry?;
        ensure!(
            !entry.file_type()?.is_symlink(),
            "symlinked experimental run cannot be recovered"
        );
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let run_id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("experimental run identity is not UTF-8"))?;
        // Old shared metadata need not be mode0600. The scanner bounds bytes,
        // refuses links/non-regular files and never repairs their permissions.
        let read = read_experimental_bytes;
        let record = read(&entry.path().join("run.json"))?;
        let reservation = read(&entry.path().join("managed-local.json"))?;
        total += record.as_ref().map_or(0, Vec::len) + reservation.as_ref().map_or(0, Vec::len);
        ensure!(
            total <= 32 * 1024 * 1024,
            "experimental scan byte budget exhausted"
        );
        let parsed = record
            .as_deref()
            .map(serde_json::from_slice::<Value>)
            .transpose()?;
        if reservation.is_none()
            && !parsed.as_ref().is_some_and(|value| {
                value.get("managedLocal").is_some()
                    || value.get("vcs").and_then(Value::as_str) == Some("fabric")
            })
        {
            continue;
        }
        let source = parsed
            .as_ref()
            .and_then(|value| value.get("managedLocal"))
            .and_then(|value| serde_json::from_value::<ManagedRunSource>(value.clone()).ok())
            .or_else(|| {
                reservation
                    .as_deref()
                    .and_then(|bytes| serde_json::from_slice::<ManagedRunSource>(bytes).ok())
            });
        let assignment = if let Some(fence) = source.as_ref().and_then(ManagedRunSource::fence) {
            let name = format!(
                "{}.json",
                byte_digest(&canonical_json(&(
                    fence.owner.clone(),
                    &fence.run_id,
                    &fence.attempt_id
                ))?)
            );
            let bytes = read(
                &binding
                    .home
                    .join("fabric/workspace-assignments")
                    .join(&name),
            )?;
            total += bytes.as_ref().map_or(0, Vec::len);
            ensure!(
                total <= 32 * 1024 * 1024,
                "experimental scan byte budget exhausted"
            );
            bytes.map(|bytes| (name, byte_digest(&bytes)))
        } else {
            None
        };
        plan.entries.push(ExperimentalRecoveryEntry {
            run_id,
            record_sha256: record.as_deref().map(byte_digest),
            reservation_sha256: reservation.as_deref().map(byte_digest),
            assignment,
        });
        ensure!(
            plan.entries.len() <= 128,
            "experimental recovery needs a smaller explicitly selected batch"
        );
    }
    plan.entries.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    plan.validate(binding)?;
    Ok(plan)
}
pub(crate) fn persist_recovery_plan(
    plan: &ExperimentalRecoveryPlan,
    binding: &ManagedProjectNamespace,
) -> Result<()> {
    plan.validate(binding)?;
    ensure!(
        inspect_experimental(binding)? == *plan,
        "experimental recovery metadata changed before preparation"
    );
    create_directory(&binding.directory.join("experimental-recovery"))?;
    create_directory(&plan.directory()?)?;
    immutable(&plan.directory()?.join("plan.json"), plan)
}
pub(crate) fn preserve_experimental(
    store: &ManagedStore,
    plan: &ExperimentalRecoveryPlan,
) -> Result<()> {
    plan.validate(store.binding())?;
    ensure!(
        inspect_experimental(store.binding())? == *plan,
        "experimental recovery metadata changed"
    );
    let root = plan.directory()?;
    for entry in &plan.entries {
        let directory = root.join(digest(&entry.run_id));
        create_directory(&directory)?;
        for (name, expected) in [
            ("run.json", &entry.record_sha256),
            ("managed-local.json", &entry.reservation_sha256),
        ] {
            if let Some(expected) = expected {
                let source = store
                    .binding()
                    .repository
                    .join(".rudder/runs")
                    .join(&entry.run_id)
                    .join(name);
                let bytes = read_experimental_bytes(&source)?
                    .context("experimental recovery source disappeared")?;
                ensure!(
                    bytes.len() <= RECORD_LIMIT && &byte_digest(&bytes) == expected,
                    "experimental recovery source changed"
                );
                frozen_bytes(&directory.join(name), &bytes)?;
            }
        }
        if let Some((name, expected)) = &entry.assignment {
            let bytes = read_experimental_bytes(
                &store
                    .binding()
                    .home
                    .join("fabric/workspace-assignments")
                    .join(name),
            )?
            .context("experimental assignment disappeared")?;
            ensure!(
                &byte_digest(&bytes) == expected,
                "experimental assignment changed"
            );
            frozen_bytes(&directory.join("assignment.json"), &bytes)?;
        }
    }
    ensure!(
        inspect_experimental(store.binding())? == *plan,
        "experimental source changed while preserving recovery evidence"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedStore {
    receipt: ManagedProjectReceipt,
}
impl ManagedStore {
    pub(crate) fn open(journal: &Journal, binding: &ManagedProjectNamespace) -> Result<Self> {
        validate_namespace(binding, journal)?;
        private_directory(&binding.home.join("fabric/managed-projects"))?;
        private_directory(&binding.directory)?;
        let request: ManagedProjectPrepare = read_record(&binding.directory.join("project.json"))?
            .context("managed prepare intent missing; recovery required")?;
        let receipt: ManagedProjectReceipt = read_record(&binding.directory.join("prepared.json"))?
            .context("managed prepared receipt missing; recovery required")?;
        receipt.validate()?;
        ensure!(
            request.namespace == *binding
                && receipt.request == request
                && journal.managed_project(&binding.project_id)?.as_ref() == Some(&receipt),
            "managed floor/namespace evidence differs"
        );
        Ok(Self { receipt })
    }
    pub(crate) fn binding(&self) -> &ManagedProjectNamespace {
        &self.receipt.request.namespace
    }
    pub(crate) fn receipt(&self) -> &ManagedProjectReceipt {
        &self.receipt
    }
    /// Pure location derivation from an already established trusted binding.
    pub(crate) fn run(&self, id: &str) -> Result<ManagedRunLocation> {
        ensure!(
            !id.is_empty()
                && id.len() <= 256
                && !id.chars().any(char::is_control)
                && !id.contains(['/', '\\'])
                && !matches!(id, "." | ".."),
            "invalid managed run identity"
        );
        Ok(ManagedRunLocation {
            store: self.clone(),
            run_id: id.into(),
        })
    }
    /// Off-App, bounded recovery only. Every discovered directory remains
    /// visible even when all of its evidence is missing or malformed.
    pub(crate) fn recover_runs(&self) -> Result<Vec<RecoveredManagedRun>> {
        let mut output = self.recover_catalog()?;
        self.recover_journal(&mut output, 32)?;
        Ok(output)
    }
    fn recover_journal(
        &self,
        output: &mut Vec<RecoveredManagedRun>,
        max_pages: usize,
    ) -> Result<()> {
        use rudder_engine::journal::ManagedAttemptRecovery;
        let mut latest = std::collections::BTreeMap::new();
        let mut cursor = None;
        let mut complete = false;
        let mut issue = None;
        let journal = (|| -> Result<Journal> {
            ensure!(
                self.binding().journal.canonicalize()? == self.binding().journal,
                "managed journal identity changed"
            );
            Journal::open(&self.binding().journal)
        })();
        if let Ok(journal) = journal {
            for _ in 0..max_pages.min(32) {
                let page =
                    match journal.managed_recovery_attempts(&self.receipt, cursor.as_ref(), 128) {
                        Ok(page) => page,
                        Err(_) => {
                            issue =
                                Some("managed journal recovery evidence is malformed/unavailable");
                            break;
                        }
                    };
                for observation in page.observations {
                    match observation {
                        ManagedAttemptRecovery::Observed {
                            run_id,
                            attempt_id,
                            run_epoch,
                            ..
                        } => {
                            latest.insert(run_id, (attempt_id, run_epoch));
                        }
                        ManagedAttemptRecovery::Malformed { .. } => {
                            issue = Some("managed journal contains a malformed attempt observation")
                        }
                    }
                }
                cursor = page.next;
                if cursor.is_none() {
                    complete = true;
                    break;
                }
            }
            if !complete && issue.is_none() {
                issue = Some(
                    "managed journal recovery scan budget exhausted; additional evidence remains",
                );
            }
        } else {
            issue = Some("managed journal missing or changed; recovery evidence is incomplete");
        }
        for (run_id, (attempt, epoch)) in &latest {
            let placeholder = format!("managed-quarantine-{}", digest(run_id));
            if let Some(row) = output
                .iter_mut()
                .find(|row| row.run_id == *run_id || row.run_id == placeholder)
            {
                row.run_id = run_id.clone();
                if row
                    .source
                    .fence()
                    .is_some_and(|fence| fence.attempt_id != *attempt || fence.run_epoch != *epoch)
                {
                    row.source = ManagedRunSource::Quarantined;
                    row.diagnostic = Some(
                        "private managed evidence differs from the historical observed attempt"
                            .into(),
                    );
                }
            } else {
                output.push(RecoveredManagedRun { run_id:run_id.clone(),source:ManagedRunSource::Quarantined,projection:None,location:None,
                    diagnostic:Some(format!("historical managed attempt {attempt} epoch {epoch} has no immutable reservation; explicit recovery required")) });
            }
        }
        for row in output.iter_mut() {
            if row.source.fence().is_some() && !latest.contains_key(&row.run_id) {
                row.source = ManagedRunSource::Quarantined;
                row.diagnostic = Some(
                    "private managed reservation has no matching historical journal observation"
                        .into(),
                );
            }
        }
        if let Some(reason) = issue {
            for row in output.iter_mut() {
                row.source = ManagedRunSource::Quarantined;
                row.diagnostic = Some(reason.into());
            }
            output.push(RecoveredManagedRun {
                run_id: "managed-journal-recovery-quarantine".into(),
                source: ManagedRunSource::Quarantined,
                projection: None,
                location: None,
                diagnostic: Some(reason.into()),
            });
        }
        ensure!(
            output.len() <= MAX_RUNS + 1,
            "managed recovery output bound exceeded"
        );
        Ok(())
    }
    fn recover_catalog(&self) -> Result<Vec<RecoveredManagedRun>> {
        let mut output = self.recover_experimental()?;
        let root = self.binding().directory.join("runs");
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(output),
            Err(error) => return Err(error.into()),
        };
        private_directory(&root)?;
        for (index, entry) in entries.enumerate() {
            ensure!(index < MAX_RUNS, "managed recovery run bound exceeded");
            let entry = entry?;
            let key = entry.file_name().to_string_lossy().into_owned();
            let recovered = (|| -> Result<RecoveredManagedRun> {
                ensure!(entry.file_type()?.is_dir(), "unexpected managed run entry");
                private_directory(&entry.path())?;
                let identity: RunIdentity = read_record(&entry.path().join("identity.json"))?
                    .context("managed run identity missing")?;
                ensure!(
                    identity.format_version == 1
                        && identity.project_id == self.binding().project_id
                        && digest(&identity.run_id) == key,
                    "managed run directory identity differs"
                );
                self.run(&identity.run_id)?.recover()
            })();
            output.push(recovered.unwrap_or_else(|_| RecoveredManagedRun {
                run_id: format!("managed-quarantine-{key}"),
                source: ManagedRunSource::Quarantined,
                projection: None,
                location: None,
                diagnostic: Some(
                    "managed evidence missing/malformed; explicit recovery required".into(),
                ),
            }));
        }
        Ok(output)
    }
    fn recover_experimental(&self) -> Result<Vec<RecoveredManagedRun>> {
        let root = self.binding().directory.join("experimental-recovery");
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        private_directory(&root)?;
        let mut recovered = Vec::new();
        for (index, entry) in entries.enumerate() {
            ensure!(index < 128, "experimental recovery plan bound exceeded");
            let entry = entry?;
            let plan = (|| -> Result<ExperimentalRecoveryPlan> {
                private_directory(&entry.path())?;
                let plan: ExperimentalRecoveryPlan = read_record(&entry.path().join("plan.json"))?
                    .context("experimental recovery plan missing")?;
                plan.validate(self.binding())?;
                ensure!(
                    plan.directory()? == entry.path(),
                    "experimental recovery directory differs"
                );
                Ok(plan)
            })();
            match plan {
                Ok(plan) => {
                    for item in plan.entries {
                        let directory = entry.path().join(digest(&item.run_id));
                        let projection = item.record_sha256.as_ref().and_then(|hash| {
                            read_bytes(&directory.join("run.json"), RECORD_LIMIT)
                                .ok()
                                .flatten()
                                .filter(|bytes| &byte_digest(bytes) == hash)
                                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        });
                        recovered.push(RecoveredManagedRun { run_id:format!("managed-experimental-{}",digest(&item.run_id)),source:ManagedRunSource::Quarantined,
                        projection,location:None,diagnostic:Some(format!("experimental source {:?} retained for explicit reconciliation; no automatic import/resume",item.run_id)) });
                    }
                }
                Err(_) => recovered.push(RecoveredManagedRun {
                    run_id: format!(
                        "managed-experimental-quarantine-{}",
                        entry.file_name().to_string_lossy()
                    ),
                    source: ManagedRunSource::Quarantined,
                    projection: None,
                    location: None,
                    diagnostic: Some(
                        "experimental recovery evidence incomplete; originals retained".into(),
                    ),
                }),
            }
            ensure!(
                recovered.len() <= MAX_RUNS,
                "experimental recovery row bound exceeded"
            );
        }
        Ok(recovered)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedRunLocation {
    store: ManagedStore,
    run_id: String,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunIdentity {
    format_version: u32,
    project_id: String,
    run_id: String,
    attempt_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Projection {
    format_version: u32,
    project_id: String,
    run_id: String,
    source: ManagedRunSource,
    data: Value,
}
pub(crate) struct RecoveredManagedRun {
    pub run_id: String,
    pub source: ManagedRunSource,
    pub projection: Option<Value>,
    pub location: Option<ManagedRunLocation>,
    pub diagnostic: Option<String>,
}
impl ManagedRunLocation {
    pub(crate) fn store(&self) -> &ManagedStore {
        &self.store
    }
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }
    pub(crate) fn directory(&self) -> PathBuf {
        self.store
            .binding()
            .directory
            .join("runs")
            .join(digest(&self.run_id))
    }
    fn attempt_directory(&self, attempt: &str) -> Result<PathBuf> {
        ensure!(
            uuid::Uuid::parse_str(attempt).is_ok_and(|id| id.to_string() == attempt),
            "invalid managed attempt directory"
        );
        Ok(self.directory().join("attempts").join(attempt))
    }
    fn check_fence(&self, fence: &ManagedLaunchFence) -> Result<()> {
        fence.validate()?;
        ensure!(
            fence.run_id == self.run_id
                && fence.owner.project_id == self.store.binding().project_id,
            "managed run location differs from reserved fence"
        );
        Ok(())
    }
    pub(crate) fn persist_reservation(&self, source: &ManagedRunSource) -> Result<()> {
        let ManagedRunSource::Reserved { fence, .. } = source else {
            anyhow::bail!("managed reservation phase required");
        };
        self.check_fence(fence)?;
        source.validate(&self.run_id)?;
        let runs = self.store.binding().directory.join("runs");
        create_directory(&runs)?;
        create_directory(&self.directory())?;
        immutable(
            &self.directory().join("identity.json"),
            &RunIdentity {
                format_version: 1,
                project_id: fence.owner.project_id.clone(),
                run_id: self.run_id.clone(),
                attempt_id: fence.attempt_id.clone(),
            },
        )?;
        create_directory(&self.directory().join("attempts"))?;
        let attempt = self.attempt_directory(&fence.attempt_id)?;
        create_directory(&attempt)?;
        immutable(&attempt.join("reservation.json"), source)
    }
    pub(crate) fn reservation(&self, attempt: &str) -> Result<Option<ManagedRunSource>> {
        let directory = self.attempt_directory(attempt)?;
        private_directory(&self.directory())?;
        private_directory(&self.directory().join("attempts"))?;
        private_directory(&directory)?;
        let source: Option<ManagedRunSource> = read_record(&directory.join("reservation.json"))?;
        if let Some(source) = &source {
            let ManagedRunSource::Reserved { fence, .. } = source else {
                anyhow::bail!("invalid managed reservation phase");
            };
            self.check_fence(fence)?;
            ensure!(
                fence.attempt_id == attempt,
                "managed reservation attempt differs"
            );
            source.validate(&self.run_id)?;
        }
        Ok(source)
    }
    pub(crate) fn persist_assignment(&self, assignment: &WorkspaceAssignment) -> Result<()> {
        self.check_fence(&assignment.fence)?;
        assignment.validate(&assignment.fence)?;
        let reservation = self
            .reservation(&assignment.fence.attempt_id)?
            .context("managed assignment has no durable reservation")?;
        ensure!(matches!(fs::symlink_metadata(self.attempt_directory(&assignment.fence.attempt_id)?.join("assignment2.json")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound), "managed assignment family collision");
        ensure!(
            ManagedRunSource::Assigned {
                assignment: Box::new(assignment.clone())
            }
            .matches_reservation(&reservation),
            "managed assignment differs from durable reservation"
        );
        immutable(
            &self
                .attempt_directory(&assignment.fence.attempt_id)?
                .join("assignment.json"),
            assignment,
        )
    }
    pub(crate) fn assignment(&self, attempt: &str) -> Result<Option<WorkspaceAssignment>> {
        let reservation = self
            .reservation(attempt)?
            .context("managed reservation missing")?;
        ensure!(matches!(fs::symlink_metadata(self.attempt_directory(attempt)?.join("assignment2.json")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound), "managed assignment family collision");
        let assignment: Option<WorkspaceAssignment> =
            read_record(&self.attempt_directory(attempt)?.join("assignment.json"))?;
        if let Some(assignment) = &assignment {
            self.check_fence(&assignment.fence)?;
            assignment.validate(&assignment.fence)?;
            ensure!(
                assignment.fence.attempt_id == attempt
                    && ManagedRunSource::Assigned {
                        assignment: Box::new(assignment.clone())
                    }
                    .matches_reservation(&reservation),
                "managed assignment differs from reservation"
            );
        }
        Ok(assignment)
    }
    fn recover(&self) -> Result<RecoveredManagedRun> {
        let identity: RunIdentity = read_record(&self.directory().join("identity.json"))?
            .context("managed run identity missing")?;
        let reserved = self
            .reservation(&identity.attempt_id)?
            .context("managed reservation missing")?;
        let remote_present = match fs::symlink_metadata(self.attempt_directory(&identity.attempt_id)?.join("assignment2.json")) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let (assigned, source_assigned) = if remote_present {
            (None, self.source_assignment(&identity.attempt_id)?)
        } else {
            (self.assignment(&identity.attempt_id)?, None)
        };
        let projection = (|| -> Result<Option<Projection>> {
            let Some(bytes) =
                read_bytes(&self.directory().join("projection.json"), PROJECTION_LIMIT)?
            else {
                return Ok(None);
            };
            let record: Projection = serde_json::from_slice(&bytes)?;
            ensure!(
                record.format_version == 1
                    && record.project_id == self.store.binding().project_id
                    && record.run_id == self.run_id
                    && record.source.matches_reservation(&reserved),
                "managed projection identity differs"
            );
            if let ManagedRunSource::Assigned { assignment } = &record.source {
                ensure!(
                    assigned.as_ref() == Some(assignment.as_ref()),
                    "managed projected assignment evidence missing"
                );
            }
            if let ManagedRunSource::SourceAssigned { assignment } = &record.source {
                ensure!(source_assigned.as_ref() == Some(assignment.as_ref()),
                    "managed projected source assignment evidence missing");
            }
            Ok(Some(record))
        })();
        let source = match &assigned {
            Some(assignment) => ManagedRunSource::Assigned {
                assignment: Box::new(assignment.clone()),
            },
            None => match source_assigned {
                Some(assignment) => ManagedRunSource::SourceAssigned { assignment: Box::new(assignment) },
                None => reserved,
            },
        };
        match projection {
            Ok(record) => Ok(RecoveredManagedRun {
                run_id: self.run_id.clone(),
                source,
                projection: record.map(|record| record.data),
                location: Some(self.clone()),
                diagnostic: None,
            }),
            Err(_) => Ok(RecoveredManagedRun {
                run_id: self.run_id.clone(),
                source: ManagedRunSource::Quarantined,
                projection: None,
                location: Some(self.clone()),
                diagnostic: Some(
                    "managed projection/evidence differs; explicit recovery required".into(),
                ),
            }),
        }
    }
    /// Off-App mutable projection; immutable evidence above remains authority.
    pub(crate) fn write_projection(&self, source: &ManagedRunSource, data: &Value) -> Result<()> {
        source.validate(&self.run_id)?;
        let fence = source.fence().context("managed projection fence absent")?;
        self.check_fence(fence)?;
        let reserved = self
            .reservation(&fence.attempt_id)?
            .context("managed projection reservation missing")?;
        ensure!(
            source.matches_reservation(&reserved),
            "managed projection differs from reservation"
        );
        if let ManagedRunSource::Assigned { assignment } = source {
            ensure!(
                self.assignment(&fence.attempt_id)?.as_ref() == Some(assignment.as_ref()),
                "managed projection assignment missing"
            );
        }
        if let ManagedRunSource::SourceAssigned { assignment } = source {
            ensure!(self.source_assignment(&fence.attempt_id)?.as_ref() == Some(assignment.as_ref()),
                "managed projected source assignment missing");
        }
        let bytes = canonical_json(&Projection {
            format_version: 1,
            project_id: self.store.binding().project_id.clone(),
            run_id: self.run_id.clone(),
            source: source.clone(),
            data: data.clone(),
        })?;
        ensure!(
            bytes.len() <= PROJECTION_LIMIT,
            "managed projection exceeds bound"
        );
        let directory = self.directory();
        let temporary = directory.join(format!(".projection-{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, directory.join("projection.json"))?;
        File::open(directory)?.sync_all()?;
        Ok(())
    }
    /// Pure bounded admission; success means queued, never a durability receipt.
    /// No filesystem/database/child wait occurs here or in a location's Drop.
    pub(crate) fn queue_projection(&self, source: &ManagedRunSource, data: Value) -> Result<()> {
        source.validate(&self.run_id)?;
        self.check_fence(source.fence().context("managed projection fence absent")?)?;
        ensure!(
            canonical_json(&data)?.len() <= PROJECTION_LIMIT - RECORD_LIMIT,
            "managed projection exceeds bound"
        );
        projection_queue()
            .context("managed projection worker unavailable; immutable evidence retained")?
            .try_send((self.clone(), source.clone(), data))
            .map_err(|_| {
                anyhow::anyhow!("managed projection queue full/closed; immutable evidence retained")
            })
    }
}
type ProjectionJob = (ManagedRunLocation, ManagedRunSource, Value);
fn projection_queue() -> Option<&'static SyncSender<ProjectionJob>> {
    static QUEUE: OnceLock<Option<SyncSender<ProjectionJob>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (send, receive) = mpsc::sync_channel::<ProjectionJob>(32);
            std::thread::Builder::new()
                .name("rudder-managed-projections".into())
                .spawn(move || {
                    while let Ok((location, source, data)) = receive.recv() {
                        let _ = location.write_projection(&source, &data);
                    }
                })
                .ok()
                .map(|_| send)
        })
        .as_ref()
}

#[cfg(test)]
#[path = "managed_store_tests.rs"]
pub(crate) mod tests;
