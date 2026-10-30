use super::*;
use crate::directory::EntryNode;
use crate::store::EnvironmentManifest;
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedEnvironment {
    pub key: ObjectRef,
    pub root: ObjectRef,
    pub receipt: ObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBuild {
    pub key: ObjectRef,
    pub outputs: ObjectRef,
    pub receipt: ObjectRef,
    pub observation: FrontendObservation,
    /// The frontend must still validate its graph/REAPI action identity. This
    /// metadata index does not bypass Buck2 because a previous build succeeded.
    pub requires_frontend_validation: bool,
}

pub struct BuildIndex {
    store: LocalStore,
    connection: Mutex<Connection>,
    locks: PathBuf,
    root: PathBuf,
}
impl BuildIndex {
    pub fn open(root: impl AsRef<Path>, store: LocalStore) -> Result<Self> {
        ensure!(
            rusqlite::version_number() >= 3_051_003,
            "SQLite >=3.51.3 is required for WAL safety"
        );
        fs::create_dir_all(root.as_ref())?;
        ensure!(
            fs::symlink_metadata(root.as_ref())?.is_dir(),
            "build index must be a real directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.as_ref(), fs::Permissions::from_mode(0o700))?;
        }
        let path = root.as_ref().join("index.sqlite");
        let connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        connection.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;CREATE TABLE IF NOT EXISTS build_index(format_version INTEGER NOT NULL,category TEXT NOT NULL,key_digest TEXT NOT NULL,key_json TEXT NOT NULL,receipt_json TEXT NOT NULL,PRIMARY KEY(category,key_digest));")?;
        let locks = root.as_ref().join("locks");
        fs::create_dir_all(&locks)?;
        Ok(Self {
            store,
            connection: Mutex::new(connection),
            locks,
            root: root.as_ref().canonicalize()?,
        })
    }
    pub fn store(&self) -> &LocalStore {
        &self.store
    }
    pub(super) fn root(&self) -> &Path {
        &self.root
    }
    pub(super) fn execution_guard(&self, key: &ObjectRef) -> Result<File> {
        self.key_lock("execution", key)
    }
    pub fn validate_prepared(&self, prepared: &PreparedEnvironment) -> Result<()> {
        let layout = environment_content(&self.store, &prepared.root)?.layout;
        ensure!(
            self.prepared_key_for(&prepared.key, layout)?.as_ref() == Some(prepared),
            "prepared environment lacks matching index authority"
        );
        Ok(())
    }

    /// Resolve a checkpoint's environment only through this local index's
    /// retained preparation authority. Provenance labels alone grant no hit.
    pub fn prepared_root(&self, root: &ObjectRef) -> Result<Option<PreparedEnvironment>> {
        kind(root, ObjectKind::EnvironmentRoot)?;
        let environment = environment_content(&self.store, root)?;
        let Some(preparation_key) = environment.provenance.get("preparation_key") else {
            return Ok(None);
        };
        digest(preparation_key)?;
        let key_json: Option<String> = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("build index lock failed"))?
            .query_row(
                "SELECT key_json FROM build_index WHERE category=?1 AND key_digest=?2",
                params![environment.layout.category(), preparation_key],
                |row| row.get(0),
            )
            .optional()?;
        let Some(key_json) = key_json else {
            return Ok(None);
        };
        let key: ObjectRef = serde_json::from_str(&key_json)?;
        kind(&key, ObjectKind::ActionKey)?;
        ensure!(
            key.digest == *preparation_key,
            "prepared key provenance differs from index"
        );
        let prepared = self.prepared_key_for(&key, environment.layout)?;
        ensure!(
            prepared
                .as_ref()
                .is_some_and(|prepared| prepared.root == *root),
            "environment provenance does not identify its retained prepared root"
        );
        Ok(prepared)
    }

    pub fn prepared(&self, spec: &PreparationSpec) -> Result<Option<PreparedEnvironment>> {
        self.prepared_with_layout(spec, PreparedContentLayout::LegacyV1)
    }

    pub fn prepared_with_layout(
        &self,
        spec: &PreparationSpec,
        layout: PreparedContentLayout,
    ) -> Result<Option<PreparedEnvironment>> {
        let lease = self.store.begin_staging(Duration::from_secs(300))?;
        let result = (|| {
            let key = spec.key_with_layout(&self.store, layout)?;
            self.prepared_key_for(&key, layout)
        })();
        let cleanup = self.store.abandon_staging(&lease.id);
        let result = result?;
        cleanup?;
        Ok(result)
    }

    /// One process prepares a particular key; peers wait, then reuse the
    /// complete published environment. Failed builders never update the index.
    pub fn prepare_once<F>(&self, spec: &PreparationSpec, prepare: F) -> Result<PreparedEnvironment>
    where
        F: FnOnce(&LocalStore) -> Result<ObjectRef>,
    {
        self.prepare_once_with_layout(spec, PreparedContentLayout::LegacyV1, prepare)
    }

    pub fn prepare_once_with_layout<F>(
        &self,
        spec: &PreparationSpec,
        layout: PreparedContentLayout,
        prepare: F,
    ) -> Result<PreparedEnvironment>
    where
        F: FnOnce(&LocalStore) -> Result<ObjectRef>,
    {
        let lease = self.store.begin_staging(Duration::from_secs(3600))?;
        let result = (|| {
            let key = spec.key_with_layout(&self.store, layout)?;
            let _lock = self.key_lock(layout.category(), &key)?;
            if let Some(existing) = self.prepared_key_for(&key, layout)? {
                self.store.abandon_staging(&lease.id)?;
                return Ok(existing);
            }
            let directory = prepare(&self.store)?;
            self.publish_prepared_inner(spec, &key, &directory, &lease.id, layout)
        })();
        if result.is_err() {
            let _ = self.store.abandon_staging(&lease.id);
        }
        result
    }

    pub fn publish_prepared(
        &self,
        spec: &PreparationSpec,
        directory: &ObjectRef,
    ) -> Result<PreparedEnvironment> {
        self.prepare_once(spec, |_| Ok(directory.clone()))
    }

    pub fn publish_prepared_with_layout(
        &self,
        spec: &PreparationSpec,
        directory: &ObjectRef,
        layout: PreparedContentLayout,
    ) -> Result<PreparedEnvironment> {
        self.prepare_once_with_layout(spec, layout, |_| Ok(directory.clone()))
    }

    /// Only an exact retained index receipt can supply a preparation contract.
    pub fn preparation_spec(&self, prepared: &PreparedEnvironment) -> Result<PreparationSpec> {
        self.validate_prepared(prepared)?;
        let layout = environment_content(&self.store, &prepared.root)?.layout;
        let spec: PreparationSpec =
            decode_payload(&self.store, &prepared.key, layout.key_schema())?;
        spec.validate()?;
        ensure!(
            spec.key_with_layout(&self.store, layout)? == prepared.key,
            "preparation contract differs from indexed key"
        );
        Ok(spec)
    }

    fn publish_prepared_inner(
        &self,
        spec: &PreparationSpec,
        key: &ObjectRef,
        directory: &ObjectRef,
        lease: &str,
        layout: PreparedContentLayout,
    ) -> Result<PreparedEnvironment> {
        kind(directory, ObjectKind::Directory)?;
        // The requested compiler must really exist in the prepared tree. A
        // version label pointing at a different executable is not provenance.
        for tool in spec.toolchains.values() {
            ensure!(
                file_at(&self.store, directory, &tool.executable_path)? == tool.executable,
                "prepared compiler does not match its pinned identity"
            );
        }
        let mut inputs: Vec<_> = spec.installer_inputs.values().cloned().collect();
        inputs.push(spec.base_runtime.clone());
        for tool in spec.toolchains.values() {
            inputs.extend([tool.closure.clone(), tool.executable.clone()]);
        }
        inputs.sort();
        inputs.dedup();
        let provenance = [
            ("preparation_key".into(), key.digest.clone()),
            ("policy_version".into(), spec.policy_version.clone()),
        ]
        .into_iter()
        .collect();
        let root = match layout {
            PreparedContentLayout::LegacyV1 => {
                self.store.put_environment(&EnvironmentManifest {
                    format_version: FORMAT_VERSION,
                    directory: directory.clone(),
                    platform: spec.platform.properties(),
                    installer_inputs: inputs,
                    provenance,
                })?
            }
            PreparedContentLayout::PagedChunkedV2 => crate::snapshot::stage(
                &self.store,
                lease,
                ObjectKind::EnvironmentRoot,
                &crate::snapshot::EnvironmentV2 {
                    format_version: 2,
                    directory: directory.clone(),
                    platform: spec.platform.properties(),
                    installer_inputs: inputs,
                    provenance,
                },
            )?,
        };
        let receipt = metadata(
            &self.store,
            ObjectKind::ActionResult,
            layout.receipt_schema(),
            &serde_json::json!({"key":key,"root":root}),
            vec![key.clone(), root.clone()],
        )?;
        self.publish_index(layout.category(), key, &receipt, lease, &[])?;
        Ok(PreparedEnvironment {
            key: key.clone(),
            root,
            receipt,
        })
    }

    fn prepared_key_for(
        &self,
        key: &ObjectRef,
        layout: PreparedContentLayout,
    ) -> Result<Option<PreparedEnvironment>> {
        let Some(receipt) = self.lookup(layout.category(), key)? else {
            return Ok(None);
        };
        #[derive(Deserialize)]
        struct Payload {
            key: ObjectRef,
            root: ObjectRef,
        }
        let payload: Payload = decode_payload(&self.store, &receipt, layout.receipt_schema())?;
        ensure!(payload.key == *key, "prepared key/index mismatch");
        kind(&payload.root, ObjectKind::EnvironmentRoot)?;
        ensure!(
            environment_content(&self.store, &payload.root)?.layout == layout,
            "prepared layout differs from indexed category"
        );
        Ok(Some(PreparedEnvironment {
            key: key.clone(),
            root: payload.root,
            receipt,
        }))
    }

    pub fn lookup_build(&self, contract: &ActionContract) -> Result<Option<StoredBuild>> {
        contract.validate()?;
        if !contract.policy.cacheable() {
            return Ok(None);
        }
        let lease = self.store.begin_staging(Duration::from_secs(300))?;
        let result = (|| {
            let key = contract.key(&self.store)?;
            let Some(receipt) = self.lookup("action", &key)? else {
                return Ok(None);
            };
            #[derive(Deserialize)]
            struct Payload {
                key: ObjectRef,
                outputs: ObjectRef,
                observation: FrontendObservation,
            }
            let payload: Payload =
                decode_payload(&self.store, &receipt, "frontend-build-receipt-v1")?;
            ensure!(payload.key == key, "action key/index mismatch");
            Ok(Some(StoredBuild {
                key,
                outputs: payload.outputs,
                receipt,
                observation: payload.observation,
                requires_frontend_validation: true,
            }))
        })();
        let cleanup = self.store.abandon_staging(&lease.id);
        let result = result?;
        cleanup?;
        Ok(result)
    }

    pub fn record_build(&self, build: &QualifiedBuild) -> Result<StoredBuild> {
        build.contract.validate()?;
        self.validate_prepared(&build.prepared)?;
        ensure!(
            build.contract.environment_root == build.prepared.root,
            "qualified build and retained prepared environment differ"
        );
        ensure!(
            build.contract.policy.cacheable(),
            "effectful build results cannot enter the reusable action index"
        );
        kind(&build.outputs, ObjectKind::Directory)?;
        kind(&build.observation.event_log, ObjectKind::Blob)?;
        kind(&build.observation.build_report, ObjectKind::Blob)?;
        ensure!(
            build.observation.frontend == build.contract.frontend,
            "unqualified frontend observation"
        );
        let lease = self.store.begin_staging(Duration::from_secs(300))?;
        let result = (|| {
            let key = build.contract.key(&self.store)?;
            let _lock = self.key_lock("action", &key)?;
            // Execution counts may differ on subsequent frontend validation;
            // immutable outputs for the same complete key must still match.
            if let Some(existing) = self.lookup_build(&build.contract)? {
                ensure!(
                    existing.outputs == build.outputs,
                    "same action contract produced different output bytes"
                );
                self.store.abandon_staging(&lease.id)?;
                return Ok(existing);
            }
            let refs = vec![
                key.clone(),
                build.outputs.clone(),
                build.observation.event_log.clone(),
                build.observation.build_report.clone(),
            ];
            let receipt = metadata(
                &self.store,
                ObjectKind::ActionResult,
                "frontend-build-receipt-v1",
                &serde_json::json!({"key":key,"outputs":build.outputs,"observation":build.observation}),
                refs,
            )?;
            self.publish_index(
                "action",
                &key,
                &receipt,
                &lease.id,
                std::slice::from_ref(&build.prepared.receipt),
            )?;
            Ok(StoredBuild {
                key,
                outputs: build.outputs.clone(),
                receipt,
                observation: build.observation.clone(),
                requires_frontend_validation: true,
            })
        })();
        if result.is_err() {
            let _ = self.store.abandon_staging(&lease.id);
        }
        result
    }

    fn lookup(&self, category: &str, key: &ObjectRef) -> Result<Option<ObjectRef>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("build index lock failed"))?;
        let row:Option<(i64,String,String)>=connection.query_row("SELECT format_version,key_json,receipt_json FROM build_index WHERE category=?1 AND key_digest=?2",params![category,key.digest],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let Some((version, stored_key, receipt)) = row else {
            return Ok(None);
        };
        ensure!(
            version == FORMAT_VERSION as i64
                && serde_json::from_str::<ObjectRef>(&stored_key)? == *key,
            "invalid build index entry"
        );
        let receipt: ObjectRef = serde_json::from_str(&receipt)?;
        ensure!(
            self.store.retention()?.pins.get(&pin(category, key)) == Some(&receipt),
            "build index lacks durable retention authority"
        );
        // Publication verified and pinned the whole closure. Verify this
        // descriptor now; output reads/materialization verify object bytes.
        // Avoid rereading an entire compiler SDK on each metadata lookup.
        self.store.read_object(&receipt)?;
        Ok(Some(receipt))
    }

    fn publish_index(
        &self,
        category: &str,
        key: &ObjectRef,
        receipt: &ObjectRef,
        staging: &str,
        retained_roots: &[ObjectRef],
    ) -> Result<()> {
        // Pin the full verified closure BEFORE acknowledging the SQL index.
        // A crash after this point can leak a conservative pin, never publish
        // a pointer to data GC is allowed to remove. Index errors keep the pin.
        self.store
            .publish_extending(staging, &pin(category, key), receipt, retained_roots)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("build index lock failed"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO build_index(format_version,category,key_digest,key_json,receipt_json) VALUES(?1,?2,?3,?4,?5)",params![FORMAT_VERSION,category,key.digest,serde_json::to_string(key)?,serde_json::to_string(receipt)?])?;
        tx.commit()?;
        Ok(())
    }
    fn key_lock(&self, category: &str, key: &ObjectRef) -> Result<File> {
        digest(&key.digest)?;
        let path = self.locks.join(format!("{category}-{}", key.digest));
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        file.lock_exclusive()?;
        Ok(file)
    }
}
fn pin(category: &str, key: &ObjectRef) -> String {
    format!("build/{category}/{}", key.digest)
}
fn file_at(store: &LocalStore, directory: &ObjectRef, path: &str) -> Result<ObjectRef> {
    relative(path)?;
    let mut root = directory.clone();
    let parts: Vec<_> = path.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        let entry = crate::tree::lookup(store, &root, part.as_bytes())?
            .context("prepared tool is missing")?;
        match entry.node {
            EntryNode::Directory { object } if index + 1 < parts.len() => root = object,
            EntryNode::File { object, .. } if index + 1 == parts.len() => return Ok(object),
            _ => anyhow::bail!("prepared compiler path is not an ordinary file"),
        }
    }
    anyhow::bail!("prepared compiler path is empty")
}
