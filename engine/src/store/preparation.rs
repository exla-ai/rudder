//! Lock-held CAS operations for the additive retained BuildIndex path. No
//! public store method is called while this owner holds GC. Existing retention
//! encoding and typed edge checks remain unchanged. Errors keep actual IO and
//! GC descriptors; neither Drop nor an elapsed staging lease performs cleanup.
use super::*;
use std::{cell::RefCell, sync::Arc};

const MAX_REFS: usize = 65_536;
const MAX_METADATA: u64 = 1024 * 1024;
const MAX_METADATA_WORK: u64 = 64 * 1024 * 1024;
const MAX_RETENTION: u64 = 16 * 1024 * 1024;

pub(crate) struct PreparationStore {
    store: LocalStore,
    original: Arc<dyn ImageImportFence>,
    pub(crate) gc: Option<File>,
    pub(crate) gc_acquired: bool,
    pub(crate) lease: Option<StagingLease>,
    pub(crate) retention_intent: Option<RetentionState>,
    pub(crate) pin_intent: Option<(String, ObjectRef)>,
    pub(crate) pin_generation: Option<u64>,
    pub(crate) pending_file: Option<File>,
    pub(crate) pending_path: Option<PathBuf>,
    directory_io: Option<File>,
    retained: Option<RetentionState>,
    metadata_work: u64,
    failed: bool,
    #[cfg(test)] pub(crate) fail_after_pin_rename: bool,
}
impl PreparationStore {
    /// Pure staging, before opening any descriptor or mutating the store.
    pub(crate) fn capture(store: LocalStore, original: Arc<dyn ImageImportFence>) -> Self {
        Self { store, original, gc: None, gc_acquired: false, lease: None,
            retention_intent: None, pin_intent: None, pin_generation: None,
            pending_file: None, pending_path: None, directory_io: None,
            retained: None, metadata_work: 0, failed: false,
            #[cfg(test)] fail_after_pin_rename: false }
    }
    fn check(&self) -> Result<()> {
        ensure!(!self.failed, "original preparation store failed; retain owner");
        ensure!(!self.original.remaining()?.is_zero(), "original preparation store expired"); Ok(())
    }
    fn locked(&self) -> Result<()> {
        self.check()?; ensure!(self.gc_acquired, "original preparation GC lock absent"); Ok(())
    }
    pub(crate) fn fail(&mut self) { self.failed = true; }
    pub(crate) fn acquire(&mut self) -> Result<()> {
        self.check()?; ensure!(self.gc.is_none(), "preparation GC acquisition already attempted");
        for path in [&self.store.root, &self.store.root.join("objects"), &self.store.root.join("tmp")] {
            self.check()?; ensure_directory(path)?;
        }
        self.gc = Some(open_file(&self.store.root.join("lock"), true, false)?);
        loop {
            self.check()?;
            match FileExt::try_lock_exclusive(self.gc.as_ref().unwrap()) {
                Ok(()) => { self.gc_acquired = true; self.check()?; break; }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(self.original.remaining()?.min(Duration::from_millis(1)));
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.retained = Some(self.read_retention()?); Ok(())
    }
    fn read_path(&mut self, path: &Path, limit: u64) -> Result<Vec<u8>> {
        self.locked()?; ensure!(self.pending_file.is_none(), "original preparation IO still pending");
        self.pending_file = Some(open_file(path, false, false)?);
        let size = self.pending_file.as_ref().unwrap().metadata()?.len();
        ensure!(size <= limit, "preparation metadata exceeds admitted byte bound");
        let mut bytes = Vec::with_capacity(usize::try_from(size)?);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            self.check()?;
            let count = self.pending_file.as_mut().unwrap().read(&mut buffer)?;
            if count == 0 { break; }
            ensure!((bytes.len() as u64).saturating_add(count as u64) <= size, "preparation object grew");
            bytes.extend_from_slice(&buffer[..count]);
        }
        ensure!(bytes.len() as u64 == size, "preparation object ended early");
        self.check()?; self.pending_file.take(); Ok(bytes)
    }
    pub(crate) fn read(&mut self, reference: &ObjectRef) -> Result<Vec<u8>> {
        reference.validate()?;
        ensure!(reference.size_bytes <= MAX_METADATA, "preparation metadata exceeds profile");
        self.metadata_work = self.metadata_work.checked_add(reference.size_bytes).context("preparation read work overflow")?;
        ensure!(self.metadata_work <= MAX_METADATA_WORK, "preparation metadata work exceeds profile");
        let path = self.store.object_path(reference)?;
        ensure_directory(path.parent().context("preparation shard absent")?)?;
        let bytes = self.read_path(&path, MAX_METADATA)?;
        reference.verify(&bytes)?; references_from_bytes(reference.kind, &bytes)?; self.check()?; Ok(bytes)
    }
    fn read_retention(&mut self) -> Result<RetentionState> {
        let bytes = self.read_path(&self.store.root.join("retention.json"), MAX_RETENTION)?;
        let envelope: StateEnvelope = serde_json::from_slice(&bytes)?;
        let encoded = canonical_json(&envelope.state)?;
        ensure!(ObjectRef::from_bytes(ObjectKind::Blob, &encoded).digest == envelope.checksum,
            "retention checksum mismatch; preparation refused");
        ensure!(matches!(envelope.state.format_version, 1 | 2), "unsupported retention format");
        if envelope.state.format_version == 2 { object_sets::validate_presence(&self.store)?; }
        for (name, reference) in &envelope.state.pins { validate_pin(name)?; reference.validate()?; }
        for (id, lease) in &envelope.state.staging { ensure!(*id == lease.id && !id.is_empty(), "invalid original staging state"); }
        image_import::validate_holds(&envelope.state.image_import_holds)?;
        self.check()?; Ok(envelope.state)
    }
    pub(crate) fn pin_matches(&self, name: &str, reference: &ObjectRef) -> Result<bool> {
        self.locked()?; Ok(self.retained.as_ref().context("retention snapshot absent")?.pins.get(name) == Some(reference))
    }
    pub(crate) fn pin_present(&self, name: &str) -> Result<bool> {
        self.locked()?; Ok(self.retained.as_ref().context("retention snapshot absent")?.pins.contains_key(name))
    }
    /// Current pins, not caller claims, permit metadata-only traversal of
    /// immutable retained content. This matches publish_extending's corruption
    /// model: future content reads still verify raw payload SHA/size.
    pub(crate) fn covered_inputs(&mut self, anchors: &[ObjectRef]) -> Result<BTreeSet<ObjectRef>> {
        self.locked()?; ensure!(!anchors.is_empty() && anchors.len() <= 1024, "preparation anchor bound");
        let retained = self.retained.as_ref().context("retention snapshot absent")?;
        for root in anchors {
            root.validate()?;
            ensure!(retained.pins.values().any(|r| r == root), "preparation anchor lacks current pin");
        }
        let mut seen = BTreeSet::new(); let mut pending = anchors.to_vec();
        while let Some(reference) = pending.pop() {
            self.check()?; reference.validate()?;
            if !seen.insert(reference.clone()) { continue; }
            ensure!(seen.len() <= MAX_REFS, "preparation closure exceeds profile");
            if reference.kind != ObjectKind::Blob {
                let bytes = self.read(&reference)?;
                pending.extend(self.edges(&reference, &bytes)?);
                ensure!(pending.len() <= MAX_REFS * 4, "preparation frontier exceeds profile");
            }
        }
        self.check()?; Ok(seen)
    }
    fn edges(&mut self, reference: &ObjectRef, bytes: &[u8]) -> Result<Vec<ObjectRef>> {
        let edges = references_from_bytes(reference.kind, bytes)?;
        match reference.kind {
            ObjectKind::RunnableRoot => {
                let parent = crate::snapshot::decode_runnable(bytes)?;
                for child in std::iter::once(&parent.source).chain(parent.environment.as_ref()) {
                    self.check()?; let child_bytes = self.read(child)?;
                    crate::snapshot::validate_child(bytes, child, &child_bytes)?;
                }
            }
            ObjectKind::File => {
                let parent = crate::content::decode_file_node(bytes)?;
                for (index, child) in parent.children.iter().enumerate() {
                    self.check()?;
                    if child.kind == ObjectKind::File {
                        let child_bytes = self.read(child)?;
                        crate::content::validate_file_child(&parent, index, &crate::content::decode_file_node(&child_bytes)?)?;
                    }
                }
            }
            ObjectKind::Directory | ObjectKind::DirectoryShard => {
                let paged = reference.kind == ObjectKind::DirectoryShard
                    || serde_json::from_slice::<serde_json::Value>(bytes)?.get("format_version").and_then(serde_json::Value::as_u64) == Some(2);
                if paged {
                    for child in edges.iter().collect::<BTreeSet<_>>() {
                        self.check()?;
                        if matches!(child.kind, ObjectKind::File | ObjectKind::DirectoryShard) {
                            let child_bytes = self.read(child)?;
                            crate::directory::validate_child(reference.kind, bytes, child, &child_bytes)?;
                        }
                    }
                }
            }
            _ => {},
        }
        self.check()?; Ok(edges)
    }
    pub(crate) fn file_at(&mut self, directory: &ObjectRef, path: &str) -> Result<ObjectRef> {
        let parts: Vec<_> = path.split('/').collect();
        ensure!(!parts.is_empty() && parts.len() <= 128 && path.len() <= 4096, "prepared tool path bound");
        let mut root = directory.clone();
        for (index, part) in parts.iter().enumerate() {
            self.check()?; let bytes = self.read(&root)?;
            let entry = if serde_json::from_slice::<serde_json::Value>(&bytes)?.get("format_version").and_then(serde_json::Value::as_u64) == Some(2) {
                let adapter = ReadAdapter(RefCell::new(&mut *self));
                crate::directory::PagedDirectory::new(&adapter).lookup(&root, part.as_bytes())?
            } else {
                let manifest: DirectoryManifest = serde_json::from_slice(&bytes)?;
                manifest.entries.into_iter().find(|e| e.name_hex == hex::encode(part.as_bytes()))
                    .map(|e| crate::directory::DirectoryEntry { name_hex: e.name_hex, node: match e.node {
                        EntryNode::File { object, mode } => crate::directory::EntryNode::File { logical_size: object.size_bytes, object, mode },
                        EntryNode::Directory { object } => crate::directory::EntryNode::Directory { object },
                        EntryNode::Symlink { target_hex } => crate::directory::EntryNode::Symlink { target_hex },
                    } })
            }.context("prepared tool missing from actual directory")?;
            match entry.node {
                crate::directory::EntryNode::Directory { object } if index + 1 < parts.len() => root = object,
                crate::directory::EntryNode::File { object, .. } if index + 1 == parts.len() => return Ok(object),
                _ => bail!("prepared tool is not an ordinary file"),
            }
        }
        bail!("prepared tool path empty")
    }
    pub(crate) fn begin_staging(&mut self) -> Result<()> {
        self.locked()?; ensure!(self.lease.is_none(), "original preparation staging already attempted");
        let remaining = self.original.remaining()?;
        let milliseconds = u64::try_from(remaining.as_millis())?.saturating_add(u64::from(remaining.subsec_nanos() % 1_000_000 != 0));
        let lease = StagingLease { id: Uuid::new_v4().to_string(), expires_at_ms: now_ms()?.checked_add(milliseconds.max(1)).context("preparation staging clock overflow")? };
        self.lease = Some(lease.clone());
        let mut state = self.retained.as_ref().context("retention snapshot absent")?.clone();
        ensure!(!state.staging.contains_key(&lease.id), "preparation staging collision");
        state.staging.insert(lease.id.clone(), lease); self.commit_state(state)
    }
    fn staging_current(&self) -> Result<()> {
        self.locked()?; let lease = self.lease.as_ref().context("original preparation staging absent")?;
        ensure!(self.retained.as_ref().context("retention snapshot absent")?.staging.get(&lease.id) == Some(lease)
            && lease.expires_at_ms > now_ms()?, "original preparation staging expired/changed"); Ok(())
    }
    fn sync_dir(&mut self, path: &Path) -> Result<()> {
        self.check()?;
        self.directory_io = Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path)?);
        self.directory_io.as_ref().unwrap().sync_all()?; self.check()?; self.directory_io.take(); Ok(())
    }
    pub(crate) fn put(&mut self, reference: &ObjectRef, bytes: &[u8]) -> Result<()> {
        self.staging_current()?; ensure!(reference.size_bytes <= MAX_METADATA, "new preparation metadata exceeds profile");
        reference.verify(bytes)?; references_from_bytes(reference.kind, bytes)?;
        ensure!(matches!(reference.kind, ObjectKind::ActionKey | ObjectKind::ActionResult | ObjectKind::EnvironmentRoot), "preparation only writes key/environment/receipt metadata");
        let destination = self.store.object_path(reference)?;
        let shard = destination.parent().context("preparation shard missing")?.to_path_buf();
        if !shard.try_exists()? { self.check()?; fs::create_dir(&shard)?; self.sync_dir(&self.store.root.join("objects"))?; }
        ensure_directory(&shard)?;
        if destination.symlink_metadata().is_ok() {
            ensure!(self.read(reference)? == bytes, "existing immutable preparation metadata corrupt");
            self.pending_file = Some(open_file(&destination, false, false)?);
            self.check()?; self.pending_file.as_ref().unwrap().sync_all()?; self.check()?;
            self.sync_dir(&shard)?; self.pending_file.take(); return Ok(());
        }
        self.pending_path = Some(self.store.root.join("tmp").join(Uuid::new_v4().to_string()));
        self.check()?; self.pending_file = Some(create_private(self.pending_path.as_ref().unwrap())?);
        for chunk in bytes.chunks(64 * 1024) { self.check()?; self.pending_file.as_mut().unwrap().write_all(chunk)?; }
        self.check()?; self.pending_file.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;
        self.pending_file.as_ref().unwrap().sync_all()?; self.check()?;
        fs::hard_link(self.pending_path.as_ref().unwrap(), &destination)?;
        self.check()?; self.sync_dir(&shard)?;
        fs::remove_file(self.pending_path.as_ref().unwrap())?; self.check()?;
        self.sync_dir(&self.store.root.join("tmp"))?;
        self.check()?; self.pending_file.take(); self.pending_path.take(); self.staging_current()
    }
    fn commit_state(&mut self, mut state: RetentionState) -> Result<()> {
        self.locked()?; ensure!(self.pending_file.is_none(), "retention write has original pending IO");
        state.generation = self.retained.as_ref().context("retention snapshot absent")?.generation.checked_add(1).context("retention generation exhausted")?;
        self.retention_intent = Some(state.clone());
        let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&state)?).digest;
        let bytes = canonical_json(&StateEnvelope { state: state.clone(), checksum })?;
        ensure!(bytes.len() as u64 <= MAX_RETENTION, "preparation retention state exceeds profile");
        self.pending_path = Some(self.store.root.join("tmp").join(Uuid::new_v4().to_string()));
        self.check()?; self.pending_file = Some(create_private(self.pending_path.as_ref().unwrap())?);
        for chunk in bytes.chunks(64 * 1024) { self.check()?; self.pending_file.as_mut().unwrap().write_all(chunk)?; }
        self.check()?; self.pending_file.as_ref().unwrap().sync_all()?; self.check()?;
        fs::rename(self.pending_path.as_ref().unwrap(), self.store.root.join("retention.json"))?;
        #[cfg(test)] if self.pin_intent.is_some() && self.fail_after_pin_rename { bail!("injected original pin directory sync failure"); }
        self.check()?; self.sync_dir(&self.store.root.clone())?; self.sync_dir(&self.store.root.join("tmp"))?;
        self.check()?; self.retained = Some(state); self.pending_file.take(); self.pending_path.take(); Ok(())
    }
    pub(crate) fn publish(&mut self, name: &str, root: &ObjectRef, covered: &BTreeSet<ObjectRef>) -> Result<u64> {
        self.staging_current()?; validate_pin(name)?;
        self.pin_intent = Some((name.to_owned(), root.clone()));
        ensure!(!self.retained.as_ref().unwrap().pins.contains_key(name), "unindexed preparation pin already exists; original recovery required");
        // New parents are validated even when all their payload inputs already
        // have current immutable retention. Only new metadata needs fsync.
        let mut seen = BTreeSet::new(); let mut pending = vec![root.clone()];
        while let Some(reference) = pending.pop() {
            self.check()?; reference.validate()?;
            if covered.contains(&reference) || !seen.insert(reference.clone()) { continue; }
            ensure!(seen.len() <= 16, "new preparation closure exceeds metadata-only profile");
            ensure!(reference.kind != ObjectKind::Blob, "new unretained compiler payload refused");
            let bytes = self.read(&reference)?; pending.extend(self.edges(&reference, &bytes)?);
            ensure!(pending.len() <= MAX_REFS * 4, "preparation publication frontier bound");
            self.pending_file = Some(open_file(&self.store.object_path(&reference)?, false, false)?);
            self.check()?; self.pending_file.as_ref().unwrap().sync_all()?; self.check()?; self.pending_file.take();
        }
        self.staging_current()?;
        let mut state = self.retained.as_ref().unwrap().clone();
        state.pins.insert(name.to_owned(), root.clone()); state.staging.remove(&self.lease.as_ref().unwrap().id);
        self.commit_state(state)?;
        let generation = self.retained.as_ref().unwrap().generation; self.pin_generation = Some(generation); self.check()?; Ok(generation)
    }
    pub(crate) fn verify_pin_current(&mut self, name: &str, root: &ObjectRef) -> Result<()> {
        self.locked()?; let state = self.read_retention()?;
        ensure!(state.pins.get(name) == Some(root), "original prepared pin absent/changed");
        self.check()?; Ok(())
    }
    /// Called only after known original SQLite commit and all postchecks.
    pub(crate) fn release_known(&mut self) {
        self.gc_acquired = false; self.gc.take();
    }
}
struct ReadAdapter<'a>(RefCell<&'a mut PreparationStore>);
impl crate::directory::DirectoryStore for ReadAdapter<'_> {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> { self.0.borrow_mut().read(reference) }
    fn put(&self, _kind: ObjectKind, _bytes: &[u8]) -> Result<ObjectRef> { bail!("preparation lookup is read-only") }
}
