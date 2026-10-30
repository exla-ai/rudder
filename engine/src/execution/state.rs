//! Local receipt authority, independent of the project journal and CAS pins.
use super::*;
use anyhow::{ensure, Context};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub(crate) struct State {
    pub directory: PathBuf,
    pub record: ExecutionRecord,
    pub is_new: bool,
    _lock: File,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    record: ExecutionRecord,
    checksum: String,
}
impl State {
    pub fn open(spec: &ExecutionSpec) -> Result<Self> {
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
            m.is_dir() && !m.file_type().is_symlink(),
            "execution root is not a real directory"
        );
        let expected = crate::protocol::canonical_json(
            &serde_json::json!({"format_version":1,"manager_root":spec.manager_root.canonicalize()?,"store_root":spec.store_root.canonicalize()?}),
        )?;
        if marker.exists() {
            ensure!(
                fs::read(&marker)? == expected,
                "execution manager binding differs"
            );
        } else {
            ensure!(
                fs::read_dir(root)?.next().is_none(),
                "refusing nonempty non-execution directory"
            );
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&marker)?;
            f.write_all(&expected)?;
            f.sync_all()?;
            File::open(root)?.sync_all()?;
        }
        for name in ["runs"] {
            super::root::private_dir(&root.join(name))?;
        }
        let directory = root.join("runs").join(&spec.execution_id);
        super::root::private_dir(&directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("owner.lock"))?;
        lock.try_lock_exclusive()
            .context("execution already has a live supervisor")?;
        let digest = hex::encode(Sha256::digest(crate::protocol::canonical_json(spec)?));
        let path = directory.join("record.json");
        let is_new = !path.try_exists()?;
        let record = if !is_new {
            let envelope: Envelope = serde_json::from_slice(&fs::read(path)?)?;
            ensure!(
                hex::encode(Sha256::digest(crate::protocol::canonical_json(
                    &envelope.record
                )?)) == envelope.checksum,
                "execution record checksum differs"
            );
            let r = envelope.record;
            ensure!(
                r.request_digest == digest
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
            _lock: lock,
        };
        if !state.directory.join("record.json").exists() {
            state.save()?;
        }
        Ok(state)
    }
    pub fn save(&self) -> Result<()> {
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
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(temp, self.directory.join("record.json"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}
