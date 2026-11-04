//! Descendant of the existing retained index implementation. Reuses its SAME
//! original SQLite keeper/HAS_MOVED witness, WAL/FULL transaction and bounded
//! GC metadata writer; it never changes the canonical BuildIndex root.
use super::*;
use crate::build::backed_preparation::{self as api, BackedPreparedEnvironment,
    BackedPreparationAcknowledgment, RetainedBackedPreparation, PriorAcknowledgment,
    BACKED_PREPARATION_CATEGORY};
use crate::directory::{DirectoryStore, EntryNode as PagedEntry, PagedDirectory};
use crate::store::backed_inputs::VerifiedBakedInputs;
use std::cell::RefCell;

impl BuildIndex {
    /// No installer runs. All local anchors must have actual current ordinary
    /// pins. The spec retains model/image/network inputs. Warm reuse requires
    /// the genuine previous ACK and SAME actual index/input owners; a SELECT
    /// cannot certify an unknown original COMMIT or restart reattachment.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_backed_retaining(
        self: &Arc<Self>, out: &mut Option<RetainedBackedPreparation>,
        spec: PreparationSpec, layout: PreparedContentLayout, directory: ObjectRef,
        local_anchors: Vec<ObjectRef>, sources: Vec<Arc<VerifiedBakedInputs>>,
        prior: Option<&BackedPreparationAcknowledgment>, original: FinalizationBudget,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        ensure!(out.is_none(), "original backed preparation output occupied");
        *out = Some(RetainedBackedPreparation::stage_backed(self.clone(), spec, layout,
            directory, local_anchors, sources, prior, original, cancelled));
        out.as_mut().unwrap().run_backed()
    }
}

impl RetainedBackedPreparation {
    #[allow(clippy::too_many_arguments)]
    fn stage_backed(index: Arc<BuildIndex>, spec: PreparationSpec, layout: PreparedContentLayout,
        directory: ObjectRef, local_anchors: Vec<ObjectRef>, sources: Vec<Arc<VerifiedBakedInputs>>,
        prior: Option<&BackedPreparationAcknowledgment>, original: FinalizationBudget,
        cancelled: Arc<AtomicBool>) -> Self {
        Self {
            core: RetainedPreparation::stage(index, spec, layout, directory, local_anchors, original, cancelled),
            sources, prior: prior.map(|p| PriorAcknowledgment { prepared: p.prepared.clone(),
                index: p.index.clone(), sources: p.sources.clone(), retention: p.retention.clone() }),
            bindings: Vec::new(), prepared: None, publication: None, retention: None,
            read: None, metadata: BTreeMap::new(), metadata_bytes: 0, ack: None,
            #[cfg(test)] fault: None,
        }
    }
}

impl RetainedBackedPreparation {
    fn check_backed(&self) -> Result<()> {
        self.core.check()?;
        for source in &self.sources { source.current(&*self.core.clock)?; }
        if let Some(file) = &self.core.key_file {
            let key = self.prepared.as_ref().context("backed key not encoded")?;
            let named = fs::symlink_metadata(self.core.index.locks.join(format!(
                "{}-{}", BACKED_PREPARATION_CATEGORY, key.key.digest)))?;
            let held = file.metadata()?;
            ensure!(Inode::of(&held) == Inode::of(&named) && held.is_file()
                && !named.file_type().is_symlink() && held.nlink() == 1 && held.len() == 0
                && held.uid() == unsafe { libc::geteuid() } && held.mode() & 0o022 == 0,
                "original backed preparation key inode changed/unsafe");
        }
        self.core.clock.remaining()?; Ok(())
    }

    pub(in crate::build) fn run_backed(&mut self) -> Result<()> {
        ensure!(self.core.status == RetainedPreparationStatus::Captured,
            "original backed preparation cannot retry");
        if let Err(error) = self.run_backed_inner() {
            self.core.status = RetainedPreparationStatus::Failed;
            if let Some(store) = &mut self.core.store { store.fail(); }
            self.core.failure = Some(error);
            anyhow::bail!("original backed preparation failed; retain whole carrier");
        }
        Ok(())
    }

    fn encode_backed(&mut self) -> Result<()> {
        ensure!(self.core.layout == PreparedContentLayout::PagedChunkedV2,
            "backed preparation requires the explicit paged/chunked layout");
        ensure!((1..=4).contains(&self.sources.len()), "backed input owner count exceeds profile");
        // Reuse the existing bounded scalar/spec/effect validation. This only
        // encodes DATA; its ordinary descriptor is replaced before any IO.
        self.core.encode()?;
        self.core.anchors.sort(); self.core.anchors.dedup();
        for value in [&self.core.spec.platform.os, &self.core.spec.platform.architecture,
            &self.core.spec.platform.abi, &self.core.spec.policy_version] {
            ensure!(!value.trim().is_empty() && !value.contains('\0'), "invalid backed preparation identity");
        }
        for (key, value) in &self.core.spec.platform.properties {
            ensure!(!key.contains('\0') && !value.contains('\0'), "invalid backed platform property");
        }
        for (name, value) in [("os", &self.core.spec.platform.os),
            ("architecture", &self.core.spec.platform.architecture), ("abi", &self.core.spec.platform.abi)] {
            ensure!(self.core.spec.platform.properties.get(name).is_none_or(|v| v == value),
                "conflicting preparation platform property");
        }
        let mut bindings = BTreeMap::new();
        for source in &self.sources {
            source.current(&*self.core.clock)?;
            let binding = source.binding_data()?.clone();
            ensure!(bindings.insert(binding.id_data()?, binding).is_none(), "duplicate baked input owner");
        }
        self.bindings = bindings.into_values().collect();
        let mut inputs = self.core.spec.installer_inputs.values().cloned().collect::<Vec<_>>();
        inputs.push(self.core.directory.clone());
        for (name, tool) in &self.core.spec.toolchains {
            ensure!(!name.trim().is_empty() && !name.contains('\0') && !tool.version.trim().is_empty()
                && !tool.version.contains('\0') && !tool.executable_path.contains('\0'), "invalid prepared tool identity/path");
            for (key, value) in &tool.configuration {
                ensure!(!key.contains('\0') && !value.contains('\0'), "invalid prepared tool configuration");
            }
            inputs.extend([tool.closure.clone(), tool.executable.clone()]);
        }
        for binding in &self.bindings { inputs.extend([binding.installer_input.clone(), binding.manifest.clone()]); }
        for root in &self.core.anchors {
            root.validate()?;
            ensure!(matches!(root.kind, ObjectKind::Blob | ObjectKind::File | ObjectKind::Directory),
                "backed preparation local anchors must contain content only");
            inputs.push(root.clone());
        }
        inputs.sort(); inputs.dedup();
        let key_bytes = encode_metadata(api::KEY_SCHEMA, &serde_json::json!({
            "format_version": 1, "layout": self.core.layout, "preparation": self.core.spec,
            "backings": self.bindings, "local_anchors": self.core.anchors,
        }), inputs.clone())?;
        let key = ObjectRef::from_bytes(ObjectKind::ActionKey, &key_bytes);
        let root_bytes = canonical_json(&crate::snapshot::EnvironmentV2 {
            format_version: 2, directory: self.core.directory.clone(),
            platform: self.core.spec.platform.properties(), installer_inputs: inputs,
            provenance: BTreeMap::from([("preparation_key".into(), key.digest.clone()),
                ("preparation_category".into(), BACKED_PREPARATION_CATEGORY.into()),
                ("policy_version".into(), self.core.spec.policy_version.clone())]),
        })?;
        let root = ObjectRef::from_bytes(ObjectKind::EnvironmentRoot, &root_bytes);
        let receipt_bytes = encode_metadata(api::RECEIPT_SCHEMA,
            &serde_json::json!({"key": key, "root": root, "backings": self.bindings}), vec![key.clone(), root.clone()])?;
        let receipt = ObjectRef::from_bytes(ObjectKind::ActionResult, &receipt_bytes);
        ensure!([key_bytes.len(), root_bytes.len(), receipt_bytes.len()].iter().all(|n| *n <= MAX_ENCODING),
            "backed preparation encoding exceeds profile");
        for (kind, raw) in [(ObjectKind::ActionKey, &key_bytes), (ObjectKind::EnvironmentRoot, &root_bytes),
            (ObjectKind::ActionResult, &receipt_bytes)] { crate::store::metadata_references(kind, raw)?; }
        // Internal reuse of the original witness does not expose an ordinary
        // PreparedEnvironment or insert anything into its category.
        self.core.expected = Some(PreparedEnvironment { key: key.clone(), root: root.clone(), receipt: receipt.clone() });
        self.core.bytes = Some([key_bytes, root_bytes, receipt_bytes]);
        self.prepared = Some(BackedPreparedEnvironment { format_version: 1, key, root, receipt, backings: self.bindings.clone() });
        self.check_backed()
    }

    fn lock_backed_key(&mut self) -> Result<()> {
        self.core.status = RetainedPreparationStatus::WaitingKey; self.check_backed()?;
        let path = self.core.index.locks.join(format!("{}-{}", BACKED_PREPARATION_CATEGORY,
            self.prepared.as_ref().unwrap().key.digest));
        self.core.key_file = Some(OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(path)?);
        loop {
            self.check_backed()?;
            match FileExt::try_lock_exclusive(self.core.key_file.as_ref().unwrap()) {
                Ok(()) => { self.core.key_locked = true; return self.check_backed(); }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.core.wait()?,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn lookup_backed(&self) -> Result<bool> {
        self.check_backed()?;
        let prepared = self.prepared.as_ref().unwrap();
        let row: Option<(i64, Option<String>, Option<String>, i64, i64)> = self.core.connection.as_ref().unwrap().query_row(
            "SELECT format_version,CASE WHEN length(CAST(key_json AS BLOB))<=?3 THEN key_json END,CASE WHEN length(CAST(receipt_json AS BLOB))<=?3 THEN receipt_json END,length(CAST(key_json AS BLOB)),length(CAST(receipt_json AS BLOB)) FROM build_index WHERE category=?1 AND key_digest=?2",
            params![BACKED_PREPARATION_CATEGORY, prepared.key.digest, MAX_ENCODING as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
        self.check_backed()?;
        let Some((version, key, receipt, key_size, receipt_size)) = row else { return Ok(false); };
        ensure!(version == FORMAT_VERSION as i64 && (0..=MAX_ENCODING as i64).contains(&key_size)
            && (0..=MAX_ENCODING as i64).contains(&receipt_size), "backed preparation index format/length mismatch");
        ensure!(serde_json::from_str::<ObjectRef>(&key.context("backed key row absent")?)? == prepared.key
            && serde_json::from_str::<ObjectRef>(&receipt.context("backed receipt row absent")?)? == prepared.receipt,
            "same backed preparation key has different descriptors");
        Ok(true)
    }

    fn select_prior(&mut self, exists: bool) -> Result<()> {
        match (&self.prior, exists) {
            (None, false) => Ok(()),
            (Some(prior), true) => {
                ensure!(Arc::ptr_eq(&prior.index, &self.core.index)
                    && &prior.prepared == self.prepared.as_ref().unwrap()
                    && prior.sources.len() == self.sources.len()
                    && self.sources.iter().all(|s| prior.sources.iter().any(|p| Arc::ptr_eq(s, p)))
                    && prior.retention.root_data() == &prior.prepared.receipt,
                    "genuine previous backed preparation owner differs");
                self.retention = Some(prior.retention.clone()); Ok(())
            }
            (None, true) => anyhow::bail!("backed row alone is DATA; original preparation/restart recovery owner required"),
            (Some(_), false) => anyhow::bail!("original acknowledged backed preparation row disappeared; no replay"),
        }
    }

    fn stage_metadata(&mut self, exists: bool) -> Result<()> {
        self.core.store = Some(PreparationStore::capture(self.core.index.store.clone(), self.core.clock.clone()));
        self.core.store.as_mut().unwrap().acquire()?; self.check_backed()?;
        let covered = if self.core.anchors.is_empty() { BTreeSet::new() }
            else { self.core.store.as_mut().unwrap().covered_inputs(&self.core.anchors)? };
        // Local per-VM network/config roots must have actual pin coverage;
        // a path/name in the spec cannot waive that provenance requirement.
        for root in self.core.spec.installer_inputs.values().chain(self.core.spec.toolchains.values().map(|t| &t.closure)) {
            ensure!(covered.contains(root) || self.sources.iter().any(|s| s.contains_data(root)),
                "preparation installer/tool input lacks actual local pin or verified backing");
        }
        if !exists {
            self.core.status = RetainedPreparationStatus::Staging;
            self.core.store.as_mut().unwrap().begin_staging()?;
            let prepared = self.prepared.as_ref().unwrap().clone();
            for (i, reference) in [&prepared.key, &prepared.root, &prepared.receipt].into_iter().enumerate() {
                self.check_backed()?; self.core.object_attempts = i + 1;
                self.core.store.as_mut().unwrap().put(reference, &self.core.bytes.as_ref().unwrap()[i])?;
            }
        }
        self.check_backed()?;
        // Known metadata writes/staging have completed. Release only this GC
        // lock, retaining the original writer+lease+IO. Actual backed publish
        // must reacquire GC under the SAME clock; nested flock is forbidden.
        self.core.store.as_mut().unwrap().release_known();
        self.check_backed()?;
        #[cfg(test)] self.point_backed(api::TestPoint::AfterMetadata)?;
        Ok(())
    }

    fn publish_backing(&mut self) -> Result<()> {
        self.check_backed()?; self.core.status = RetainedPreparationStatus::Publishing;
        let staging = self.core.staging_id_data().context("original backed metadata staging absent")?.to_owned();
        let prepared = self.prepared.as_ref().unwrap();
        self.core.pin_attempted = true;
        self.core.index.store.publish_backed_retaining(&mut self.publication, staging,
            pin(BACKED_PREPARATION_CATEGORY, &prepared.key), prepared.receipt.clone(), self.sources.clone(), self.core.clock.clone())?;
        // Stage the GENUINE original retention ACK before any fallible check
        // or SQLite INSERT. A late/panic path must keep this publication.
        self.retention = Some(self.publication.as_ref().unwrap().acknowledgment()?.clone());
        #[cfg(test)] self.point_backed(api::TestPoint::AfterRetention)?;
        self.check_backed()
    }

    fn read_actual(&mut self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.check_backed()?;
        ensure!(self.read.is_none(), "original backed preparation read still pending");
        let owner = self.retention.as_ref().context("actual backed retention ACK absent")?.clone();
        owner.read_retaining(&mut self.read, reference.clone(), self.core.clock.clone())?;
        let raw = self.read.as_ref().unwrap().verified_bytes()?.to_vec();
        self.check_backed()?; self.read.take(); Ok(raw)
    }
    fn metadata_actual(&mut self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.check_backed()?;
        ensure!(matches!(reference.kind, ObjectKind::Directory | ObjectKind::DirectoryShard | ObjectKind::File)
            && reference.size_bytes <= 64 * 1024, "backed tool lookup only reads bounded content metadata");
        if let Some(raw) = self.metadata.get(reference) { return Ok(raw.clone()); }
        self.metadata_bytes = self.metadata_bytes.checked_add(reference.size_bytes).context("backed tool metadata work overflow")?;
        ensure!(self.metadata_bytes <= 64 * 1024 * 1024 && self.metadata.len() < 65_536, "backed tool metadata work bound");
        let raw = self.read_actual(reference)?; self.metadata.insert(reference.clone(), raw.clone()); Ok(raw)
    }

    fn executable_at(&mut self, directory: &ObjectRef, path: &str) -> Result<ObjectRef> {
        let parts = path.split('/').collect::<Vec<_>>();
        ensure!(!parts.is_empty() && parts.len() <= 128 && path.len() <= 4096
            && parts.iter().all(|p| !p.is_empty() && *p != "." && *p != ".." && !p.contains('\0')),
            "backed executable path bound/components");
        let mut root = directory.clone();
        for (index, part) in parts.iter().enumerate() {
            self.check_backed()?;
            let adapter = BackedDirectory(RefCell::new(&mut *self));
            let entry = PagedDirectory::new(&adapter).lookup(&root, part.as_bytes())?
                .context("backed executable missing from actual prepared directory")?;
            match entry.node {
                PagedEntry::Directory { object } if index + 1 < parts.len() => root = object,
                PagedEntry::File { object, mode, .. } if index + 1 == parts.len() => {
                    ensure!(mode & 0o111 != 0, "backed prepared tool is not executable"); return Ok(object);
                }
                _ => anyhow::bail!("backed prepared executable is not an ordinary declared file"),
            }
        }
        anyhow::bail!("empty backed executable path")
    }

    fn validate_actual_preparation(&mut self) -> Result<()> {
        let prepared = self.prepared.as_ref().unwrap().clone();
        let owner = self.retention.as_ref().context("backed pin ACK missing")?;
        ensure!(owner.root_data() == &prepared.receipt, "actual backed pin names another preparation");
        let ids = self.bindings.iter().map(|b| b.id_data()).collect::<Result<BTreeSet<_>>>()?;
        ensure!(owner.record_data().backing_ids.iter().cloned().collect::<BTreeSet<_>>() == ids,
            "actual backed publication does not retain every preparation backing");
        for root in &self.core.anchors {
            ensure!(owner.record_data().local_objects.contains(root), "actual network/local anchor is not locally retained");
        }
        for (i, reference) in [&prepared.key, &prepared.root, &prepared.receipt].into_iter().enumerate() {
            let raw = self.read_actual(reference)?;
            ensure!(raw == self.core.bytes.as_ref().unwrap()[i], "backed preparation actual descriptor differs");
        }
        let tools = self.core.spec.toolchains.values().cloned().collect::<Vec<_>>();
        for tool in tools {
            let directory = self.core.directory.clone();
            ensure!(self.executable_at(&directory, &tool.executable_path)? == tool.executable,
                "actual backed executable differs from requested tool identity");
            if tool.closure != directory {
                ensure!(self.executable_at(&tool.closure, &tool.executable_path)? == tool.executable,
                    "actual backed tool closure lacks requested executable");
            }
        }
        self.check_backed()
    }

    fn run_backed_inner(&mut self) -> Result<()> {
        self.encode_backed()?; self.core.capture_index()?; self.lock_backed_key()?; self.core.open_sqlite()?;
        self.core.status = RetainedPreparationStatus::Inspecting;
        let exists = self.lookup_backed()?;
        self.select_prior(exists)?; self.stage_metadata(exists)?;
        if !exists { self.publish_backing()?; }
        self.validate_actual_preparation()?;
        let prepared = self.prepared.as_ref().unwrap().clone();
        if !exists {
            self.check_backed()?; self.core.status = RetainedPreparationStatus::Inserting;
            self.core.sql = SqlOutcome::InsertAttempted;
            self.core.connection.as_ref().unwrap().execute(
                "INSERT INTO build_index(format_version,category,key_digest,key_json,receipt_json) VALUES(?1,?2,?3,?4,?5)",
                params![FORMAT_VERSION, BACKED_PREPARATION_CATEGORY, prepared.key.digest,
                    serde_json::to_string(&prepared.key)?, serde_json::to_string(&prepared.receipt)?])?;
            self.core.sql = SqlOutcome::Inserted;
            #[cfg(test)] self.point_backed(api::TestPoint::AfterInsert)?;
        }
        self.check_backed()?; self.core.status = RetainedPreparationStatus::Committing;
        self.core.sql = SqlOutcome::CommitAttempted;
        #[cfg(test)] if self.fault == Some(api::TestPoint::CommitHookRefusal) {
            self.core.connection.as_mut().unwrap().commit_hook(Some(|| true))?;
        }
        self.core.connection.as_ref().unwrap().execute_batch("COMMIT")?;
        self.core.sql = SqlOutcome::Committed;
        #[cfg(test)] self.point_backed(api::TestPoint::AfterCommit)?;
        self.check_backed()?; ensure!(self.lookup_backed()?, "original backed preparation row disappeared");
        // Recheck actual retention/current source AFTER original COMMIT. This
        // read is retained if post-ACK IO or the original clock fails.
        let raw = self.read_actual(&prepared.receipt)?;
        ensure!(raw == self.core.bytes.as_ref().unwrap()[2], "original committed backed receipt changed");
        self.check_backed()?;
        self.core.key_locked = false; self.core.key_file.take();
        self.ack = Some(BackedPreparationAcknowledgment { prepared, reused: exists,
            index: self.core.index.clone(), sources: self.sources.clone(),
            retention: self.retention.as_ref().unwrap().clone() });
        self.core.status = RetainedPreparationStatus::Acknowledged; Ok(())
    }

    #[cfg(test)] fn point_backed(&self, point: api::TestPoint) -> Result<()> {
        if self.fault == Some(api::TestPoint::PanicAfterRetention) && point == api::TestPoint::AfterRetention {
            panic!("injected caller unwind after actual backed retention ACK");
        }
        if (self.fault == Some(api::TestPoint::CancelAfterRetention) && point == api::TestPoint::AfterRetention)
            || (self.fault == Some(api::TestPoint::CancelAfterCommit) && point == api::TestPoint::AfterCommit) {
            self.core.original.cancel();
        }
        ensure!(self.fault != Some(point), "injected original backed preparation outcome loss"); Ok(())
    }
}

struct BackedDirectory<'a>(RefCell<&'a mut RetainedBackedPreparation>);
impl DirectoryStore for BackedDirectory<'_> {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> { self.0.borrow_mut().metadata_actual(reference) }
    fn put(&self, _: ObjectKind, _: &[u8]) -> Result<ObjectRef> { anyhow::bail!("backed preparation lookup is read-only") }
}

#[cfg(test)]
#[path = "backed_preparation_tests.rs"]
mod tests;
