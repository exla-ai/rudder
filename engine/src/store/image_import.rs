//! Deferred durability for the fixed image importer. ObjectRef results are
//! staged DATA, never per-object durability acknowledgments. A durable hold
//! blocks GC across short lock phases, including after cancellation/crash.
//! Neither elapsed time nor Drop releases that hold. The sole successful pin
//! transaction installs both roots before removing it. No replay/reopen API.
use super::*;
use crate::{content::{ContentStore, FileNode}, directory::DirectoryStore};
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_OBJECTS: usize = 65_536;
const MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const CHUNK: u64 = 4 * 1024 * 1024;
const MANIFEST: u64 = 32 * 1024 * 1024;
const FLUSH_OBJECTS: usize = 1024;
const FLUSH_BYTES: u64 = 32 * 1024 * 1024;
const MAX_HOLDS: usize = 8;
const MAX_RETENTION_BYTES:u64=16*1024*1024;

/// Restriction only. Native's adapter owns the SAME original image clock and
/// cancellation. Implementations cannot create source/project/lease authority.
pub trait ImageImportFence: Send + Sync {
    fn remaining(&self) -> Result<Duration>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageImportHold {
    format_version: u32,
    import_id: String,
    staging_id: String,
}
pub(super) fn validate_holds(holds: &BTreeMap<String, ImageImportHold>) -> Result<()> {
    ensure!(holds.len() <= MAX_HOLDS, "image import hold bound exceeded");
    for (id, hold) in holds {
        ensure!(id == &hold.import_id && hold.format_version == 1
            && Uuid::parse_str(id).is_ok_and(|v| v.to_string() == *id)
            && Uuid::parse_str(&hold.staging_id).is_ok_and(|v| v.to_string() == hold.staging_id),
            "invalid image import hold; GC refused");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageImportStats {
    pub received_calls: u64,
    pub unique_objects: u64,
    pub staged_bytes: u64,
    pub object_file_syncs: u64,
    pub directory_syncs: u64,
    pub retention_file_syncs: u64,
    pub gc_acquisitions: u64,
    pub verified_object_reads: u64,
    pub verified_bytes: u64,
    pub flushes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct PinData { import_id: String, generation: u64, pins: [(String, ObjectRef); 2] }
/// Only the actual successful commit issues this process-local result. No
/// Clone/Serde/constructor; a path or retention readback cannot recreate it.
pub struct LocalImagePinAck { data: PinData }
impl LocalImagePinAck { pub fn generation_data(&self) -> u64 { self.data.generation } }

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity { dev: u64, ino: u64, len: u64, mode: u32, mtime: (i64,i64), ctime: (i64,i64) }
fn identity(file: &File) -> Result<Identity> {
    let m = file.metadata()?;
    ensure!(m.is_file(), "image CAS inode is not regular");
    Ok(Identity {dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),
        mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase { Captured, Writing, Committing, Committed, Failed }
struct State {
    phase: Phase,
    hold: ImageImportHold,
    lease: Option<StagingLease>,
    gc: Option<File>,
    io: Option<File>,
    pending_file: Option<File>,
    pending_path: Option<PathBuf>,
    retention_intent: Option<RetentionState>,
    pin_intents: Option<[(String, ObjectRef); 2]>,
    received: BTreeSet<ObjectRef>,
    pending: BTreeSet<ObjectRef>,
    private: BTreeMap<ObjectRef, PathBuf>,
    pending_bytes: u64,
    synced: BTreeMap<ObjectRef, Identity>,
    dirty_dirs: BTreeSet<PathBuf>,
    manifest: Option<ObjectRef>,
    ack: Option<PinData>,
    graph: Option<Vec<ObjectRef>>,
    stats: ImageImportStats,
    #[cfg(test)] fail_at: Option<TestPoint>,
}
/// Retain outside catch/unwind. Errors are absorbing, keep partial files,
/// original descriptors, hold/lease/pin intents and unknown ACK. No IO in Drop.
pub struct LocalImageImport { store: LocalStore, original: Arc<dyn ImageImportFence>, state: Mutex<State> }
impl LocalImageImport {
    pub fn stage_retaining(out: &mut Option<Self>, store: LocalStore, original: Arc<dyn ImageImportFence>) -> Result<()> {
        ensure!(out.is_none(), "image import output occupied");
        original.remaining()?;
        let id = Uuid::new_v4().to_string();
        *out = Some(Self { store, original, state: Mutex::new(State {
            phase:Phase::Captured,hold:ImageImportHold {format_version:1,import_id:id,staging_id:Uuid::new_v4().to_string()},
            lease:None,gc:None,io:None,pending_file:None,pending_path:None,retention_intent:None,pin_intents:None,
            received:BTreeSet::new(),pending:BTreeSet::new(),private:BTreeMap::new(),pending_bytes:0,synced:BTreeMap::new(),dirty_dirs:BTreeSet::new(),
            manifest:None,ack:None,graph:None,stats:ImageImportStats::default(),#[cfg(test)] fail_at:None,
        }) });
        let held = out.as_ref().unwrap();
        let mut state = held.borrow()?;
        let result = (|| {
            held.acquire(&mut state)?;
            let mut retained = held.read_retention()?;
            ensure!(retained.image_import_holds.len() < MAX_HOLDS
                && !retained.image_import_holds.contains_key(&state.hold.import_id)
                && !retained.staging.contains_key(&state.hold.staging_id), "image import hold collision/full");
            let lease = StagingLease {id:state.hold.staging_id.clone(),expires_at_ms:lease_deadline(Duration::from_secs(600))?};
            state.lease = Some(lease.clone());
            retained.staging.insert(lease.id.clone(), lease);
            retained.image_import_holds.insert(state.hold.import_id.clone(), state.hold.clone());
            held.commit_state(&mut state, retained)?;
            state.phase = Phase::Writing;
            Ok(())
        })();
        held.finish(&mut state, result)
    }
    fn borrow(&self) -> Result<MutexGuard<'_, State>> {
        self.state.try_lock().map_err(|_|anyhow::anyhow!("original image import is busy/poisoned; retain owner"))
    }
    fn check(&self) -> Result<()> { ensure!(!self.original.remaining()?.is_zero(),"original image import clock expired"); Ok(()) }
    fn acquire(&self, state: &mut State) -> Result<()> {
        self.check()?;
        ensure!(state.gc.is_none(), "image import attempted reentrant GC lock");
        ensure_directory(&self.store.root)?;
        for name in ["objects","tmp"] { ensure_directory(&self.store.root.join(name))?; }
        state.gc = Some(open_file(&self.store.root.join("lock"),true,true)?);
        loop {
            self.check()?;
            match FileExt::try_lock_exclusive(state.gc.as_ref().unwrap()) {
                Ok(()) => { state.stats.gc_acquisitions += 1; self.check()?; return Ok(()); }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let remaining = self.original.remaining()?;
                    std::thread::sleep(remaining.min(Duration::from_millis(1)));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    fn retained(&self, state: &State) -> Result<RetentionState> {
        self.check()?;
        ensure!(state.gc.is_some(), "actual GC owner absent");
        let retained = self.read_retention()?;
        ensure!(retained.image_import_holds.get(&state.hold.import_id) == Some(&state.hold)
            && retained.staging.get(&state.hold.staging_id) == state.lease.as_ref(), "original image import hold/lease changed");
        ensure!(state.lease.as_ref().unwrap().expires_at_ms > now_ms()?, "original image staging expired");
        self.check()?;Ok(retained)
    }
    fn read_retention(&self)->Result<RetentionState> {
        let m=fs::symlink_metadata(self.store.root.join("retention.json"))?;
        ensure!(m.is_file()&&!m.file_type().is_symlink()&&m.len()<=MAX_RETENTION_BYTES,"image import retention state exceeds bound/is unsafe");
        self.store.read_state()
    }
    fn finish<T>(&self, state: &mut State, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => { self.check().map_err(|e| {state.phase=Phase::Failed;e})?;
                // All synchronous phase IO returned successfully. Unknown
                // phases retain this exact fd/lock instead of releasing it.
                state.gc.take(); Ok(value) }
            Err(error) => { state.phase=Phase::Failed; Err(error) }
        }
    }
    pub fn staging_id_data(&self) -> Result<String> { Ok(self.borrow()?.hold.staging_id.clone()) }
    pub fn stats_data(&self) -> Result<ImageImportStats> { Ok(self.borrow()?.stats) }
    pub fn receive(&self, reference: &ObjectRef, bytes: &[u8]) -> Result<()> { self.receive_kind(reference,bytes,false) }
    pub fn receive_manifest(&self, reference: &ObjectRef, bytes: &[u8]) -> Result<()> { self.receive_kind(reference,bytes,true) }
    fn receive_kind(&self, reference: &ObjectRef, bytes: &[u8], manifest: bool) -> Result<()> {
        let mut state = self.borrow()?;
        ensure!(state.phase==Phase::Writing, "image import not open; unknown writes must not replay");
        let result = (|| {
            self.check()?;
            object_bound(reference, manifest)?;
            reference.verify(bytes)?; references_from_bytes(reference.kind,bytes)?;
            self.check()?;
            if manifest { ensure!(reference.kind==ObjectKind::Blob && state.manifest.as_ref().is_none_or(|r|r==reference), "image manifest changed"); state.manifest=Some(reference.clone()); }
            if !state.received.contains(reference) {
                ensure!(state.received.len()<MAX_OBJECTS, "image import object bound exceeded");
                let total=state.stats.staged_bytes.checked_add(reference.size_bytes).context("image import byte overflow")?;
                ensure!(total<=MAX_TOTAL, "image import bytes exceed bound");
                state.received.insert(reference.clone()); state.stats.unique_objects+=1; state.stats.staged_bytes=total;
            }
            state.stats.received_calls+=1;
            if !state.pending.is_empty() && (state.pending.len()>=FLUSH_OBJECTS
                || state.pending_bytes.saturating_add(reference.size_bytes)>FLUSH_BYTES) { self.flush(&mut state)?; }
            self.acquire(&mut state)?; self.retained(&state)?;
            self.put_deferred(&mut state,reference,bytes)?;
            if state.pending.insert(reference.clone()) {state.pending_bytes+=reference.size_bytes;}
            Ok(())
        })();
        self.finish(&mut state,result)
    }
    fn put_deferred(&self, state:&mut State, reference:&ObjectRef, bytes:&[u8])->Result<()> {
        self.check()?;
        let destination=self.store.object_path(reference)?;
        if state.private.contains_key(reference)||destination.symlink_metadata().is_ok() {
            let existing=self.read_stream(state,reference,true,false)?;
            ensure!(existing==bytes,"existing image CAS object corrupt");
            return Ok(());
        }
        let temporary=self.store.root.join("tmp").join(Uuid::new_v4().to_string());
        state.private.insert(reference.clone(),temporary.clone());
        state.pending_path=Some(temporary.clone());
        self.check()?;
        state.pending_file=Some(create_private(&temporary)?);
        for chunk in bytes.chunks(64*1024) { self.check()?; state.pending_file.as_mut().unwrap().write_all(chunk)?; }
        self.check()?;state.pending_file.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;
        self.check()?;
        state.pending_path=None; state.pending_file.take();Ok(())
    }
    fn flush(&self, state:&mut State)->Result<()> {
        ensure!(state.gc.is_none(),"image flush is reentrant");
        self.acquire(state)?;self.retained(state)?;
        let pending:Vec<_>=state.pending.iter().cloned().collect();
        // Make every shard ancestor durable BEFORE linking any synced private
        // payload into canonical CAS. An ordinary overlapping put then sees a
        // synced inode and supplies its usual file+shard durability barriers.
        if !state.private.is_empty() {
            for reference in &pending { if state.private.contains_key(reference) {
                let destination=self.store.object_path(reference)?;let shard=destination.parent().unwrap();
                self.check()?;if !shard.try_exists()? {fs::create_dir(shard)?;}ensure_directory(shard)?;
            }}
            self.check()?;state.io=Some(File::open(self.store.root.join("objects"))?);
            sync_file(state.io.as_ref().unwrap())?;state.stats.directory_syncs+=1;self.check()?;state.io.take();
        }
        for reference in &pending {
            self.check()?;
            let source=state.private.get(reference).cloned().unwrap_or(self.store.object_path(reference)?);
            state.io=Some(open_file(&source,false,false)?);
            let before=identity(state.io.as_ref().unwrap())?;
            ensure!(before.len==reference.size_bytes,"staged image object size changed");
            self.sync_object(state,reference,before)?;
            if state.private.contains_key(reference) {
                let destination=self.store.object_path(reference)?;self.check()?;
                match fs::hard_link(&source,&destination) {
                    Ok(())=>{},
                    Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>{
                        // A concurrent ordinary import may have won. Verify
                        // that actual canonical inode; never replace it or
                        // transfer the private inode's durability to it.
                        self.read_stream_at(state,reference,&destination,false,true)?;
                    }
                    Err(e)=>return Err(e.into()),
                }
                state.dirty_dirs.insert(destination.parent().unwrap().to_path_buf());
                #[cfg(test)] self.point(state,TestPoint::ObjectLinked)?;
                self.check()?;
            }
            state.io.take();
        }
        self.sync_directories(state)?;
        for reference in &pending { if let Some(path)=state.private.get(reference).cloned() {
            self.check()?;fs::remove_file(&path)?;state.private.remove(reference);
            state.dirty_dirs.insert(self.store.root.join("tmp"));
            state.io=Some(open_file(&self.store.object_path(reference)?,false,false)?);
            let final_identity=identity(state.io.as_ref().unwrap())?;
            let synced=state.synced.get(reference).context("original synced image inode absent")?;
            ensure!((synced.dev,synced.ino,synced.len,synced.mode)==(final_identity.dev,final_identity.ino,final_identity.len,final_identity.mode),
                "published image inode differs from original durability owner");
            state.synced.insert(reference.clone(),final_identity);state.io.take();
        }}
        self.sync_directories(state)?;self.retained(state)?;
        state.pending.clear();state.pending_bytes=0;state.stats.flushes+=1;
        self.check()?;state.gc.take();Ok(())
    }
    fn sync_object(&self,state:&mut State,reference:&ObjectRef,before:Identity)->Result<()> {
        self.check()?;
        #[cfg(test)] self.point(state,TestPoint::BeforeObjectSync)?;
        sync_file(state.io.as_ref().unwrap())?;state.stats.object_file_syncs+=1;
        #[cfg(test)] self.point(state,TestPoint::AfterObjectSync)?;
        self.check()?;
        let after=identity(state.io.as_ref().unwrap())?;
        ensure!(before==after,"image CAS inode changed during durability barrier");
        state.synced.insert(reference.clone(),after);
        state.dirty_dirs.insert(self.store.object_path(reference)?.parent().unwrap().to_path_buf());
        Ok(())
    }
    fn sync_directories(&self,state:&mut State)->Result<()> {
        let directories:Vec<_>=state.dirty_dirs.iter().cloned().collect();
        for path in directories {
            self.check()?;ensure_directory(&path)?;
            state.io=Some(File::open(&path)?);
            sync_file(state.io.as_ref().unwrap())?;state.stats.directory_syncs+=1;self.check()?;
            state.io.take();state.dirty_dirs.remove(&path);
        }
        Ok(())
    }
    /// Full payload SHA on every final closure visit, never timestamp-only
    /// hash success. Keep at most one actual read/sync fd. Blob verification
    /// streams without materializing the separate 32MiB manifest.
    fn read_stream(&self,state:&mut State,reference:&ObjectRef,keep_blob:bool,durable:bool)->Result<Vec<u8>> {
        let path=state.private.get(reference).cloned().unwrap_or(self.store.object_path(reference)?);
        self.read_stream_at(state,reference,&path,keep_blob,durable)
    }
    fn read_stream_at(&self,state:&mut State,reference:&ObjectRef,path:&Path,keep_blob:bool,durable:bool)->Result<Vec<u8>> {
        self.check()?;reference.validate()?;
        ensure_directory(path.parent().unwrap())?;
        state.io=Some(open_file(path,false,false)?);
        let before=identity(state.io.as_ref().unwrap())?;
        ensure!(before.len==reference.size_bytes,"image object size differs before read");
        let mut bytes=Vec::new();let mut sha=Sha256::new();let mut total=0u64;let mut buffer=[0u8;64*1024];
        loop {
            self.check()?;let count=state.io.as_mut().unwrap().read(&mut buffer)?;self.check()?;
            if count==0 {break;}
            total=total.checked_add(count as u64).context("image read overflow")?;
            ensure!(total<=reference.size_bytes,"image object grew during read");
            sha.update(&buffer[..count]);
            if reference.kind!=ObjectKind::Blob||keep_blob {bytes.extend_from_slice(&buffer[..count]);}
        }
        ensure!(total==reference.size_bytes&&hex::encode(sha.finalize())==reference.digest,"image object digest/size differs");
        ensure!(identity(state.io.as_ref().unwrap())?==before,"image CAS inode changed during verified read");
        if reference.kind!=ObjectKind::Blob {references_from_bytes(reference.kind,&bytes)?;}
        state.stats.verified_object_reads+=1;state.stats.verified_bytes+=total;
        if durable&&state.synced.get(reference)!=Some(&before) {
            self.sync_object(state,reference,before)?;
        }
        self.check()?;state.io.take();Ok(bytes)
    }
    fn read_phase(&self,state:&mut State,reference:&ObjectRef,keep_blob:bool,durable:bool)->Result<Vec<u8>> {
        object_bound(reference,state.manifest.as_ref()==Some(reference))?;
        self.acquire(state)?;self.retained(state)?;
        let bytes=self.read_stream(state,reference,keep_blob,durable)?;
        self.retained(state)?;self.check()?;state.gc.take();Ok(bytes)
    }
    fn read_for_trait(&self,reference:&ObjectRef)->Result<Vec<u8>> {
        let mut state=self.borrow()?;
        ensure!(state.phase==Phase::Writing,"image import read outside original write phase");
        let result=(|| {object_bound(reference,state.manifest.as_ref()==Some(reference))?;
            self.read_phase(&mut state,reference,true,false)})();
        self.finish(&mut state,result)
    }
    fn edges(&self,state:&mut State,reference:&ObjectRef,bytes:&[u8])->Result<Vec<ObjectRef>> {
        let edges=references_from_bytes(reference.kind,bytes)?;
        match reference.kind {
            ObjectKind::File=>{
                let parent=crate::content::decode_file_node(bytes)?;
                for (index,child) in parent.children.iter().enumerate() {
                    self.check()?;
                    if child.kind==ObjectKind::File {
                        let child_bytes=self.read_phase(state,child,false,true)?;
                        let node=crate::content::decode_file_node(&child_bytes)?;
                        crate::content::validate_file_child(&parent,index,&node)?;
                    }
                }
            }
            ObjectKind::Directory|ObjectKind::DirectoryShard=>{
                let paged=reference.kind==ObjectKind::DirectoryShard||serde_json::from_slice::<serde_json::Value>(bytes)?
                    .get("format_version").and_then(serde_json::Value::as_u64)==Some(2);
                if paged {
                    for child in edges.iter().collect::<BTreeSet<_>>() {
                        self.check()?;
                        if matches!(child.kind,ObjectKind::File|ObjectKind::DirectoryShard) {
                            let child_bytes=self.read_phase(state,child,false,true)?;
                            crate::directory::validate_child(reference.kind,bytes,child,&child_bytes)?;
                        }
                    }
                }
            }
            ObjectKind::Blob=>{},
            _=>bail!("non-image object entered image closure"),
        }
        Ok(edges)
    }
    fn verify_closure(&self,state:&mut State,installer:&ObjectRef,manifest:&ObjectRef)->Result<()> {
        let mut seen=BTreeSet::new();let mut total=0u64;
        // Check bounds BEFORE dedup: the large manifest exception applies to
        // its separate root, never an installer child sharing the same ref.
        let mut pending=vec![(installer.clone(),false),(manifest.clone(),true)];
        while let Some((reference,is_manifest))=pending.pop() {
            self.check()?;object_bound(&reference,is_manifest)?;
            if !seen.insert(reference.clone()){continue;}
            total=total.checked_add(reference.size_bytes).context("image closure size overflow")?;
            ensure!(seen.len()<=MAX_OBJECTS&&total<=MAX_TOTAL,"image closure quota exceeded");
            let bytes=self.read_phase(state,&reference,false,true)?;
            if reference.kind!=ObjectKind::Blob {
                pending.extend(self.edges(state,&reference,&bytes)?.into_iter().map(|r|(r,false)));
                ensure!(pending.len()<=MAX_OBJECTS*4,"image closure pending frontier exceeds bound");
            }
        }
        state.graph=Some(seen.into_iter().collect());
        Ok(())
    }
    pub fn commit_pins(&self,installer_pin:&str,installer:&ObjectRef,manifest_pin:&str,manifest:&ObjectRef)->Result<LocalImagePinAck> {
        let mut state=self.borrow()?;
        ensure!(state.phase==Phase::Writing,"image pin commit already attempted/unknown");
        state.phase=Phase::Committing;
        state.pin_intents=Some([(installer_pin.into(),installer.clone()),(manifest_pin.into(),manifest.clone())]);
        let result=(|| {
            self.check()?;validate_pin(installer_pin)?;validate_pin(manifest_pin)?;
            ensure!(installer_pin!=manifest_pin&&installer.kind==ObjectKind::Directory
                &&manifest.kind==ObjectKind::Blob&&state.manifest.as_ref()==Some(manifest),"image pin roots differ from original intents");
            self.flush(&mut state)?;
            self.verify_closure(&mut state,installer,manifest)?;
            self.acquire(&mut state)?;let mut retained=self.retained(&state)?;
            self.sync_directories(&mut state)?;
            ensure!(!retained.pins.contains_key(installer_pin)&&!retained.pins.contains_key(manifest_pin),"original image pin collided");
            retained.pins.insert(installer_pin.into(),installer.clone());retained.pins.insert(manifest_pin.into(),manifest.clone());
            retained.image_import_holds.remove(&state.hold.import_id);
            retained.staging.remove(&state.hold.staging_id);
            self.commit_state(&mut state,retained)?;
            let data=PinData {import_id:state.hold.import_id.clone(),generation:state.retention_intent.as_ref().unwrap().generation,
                pins:state.pin_intents.as_ref().unwrap().clone()};
            state.ack=Some(data.clone());
            #[cfg(test)] self.point(&mut state,TestPoint::AfterPinCommit)?;
            self.check()?;state.phase=Phase::Committed;Ok(LocalImagePinAck {data})
        })();
        self.finish(&mut state,result)
    }
    /// Later read-only custody checks intentionally do not renew or reassert
    /// the elapsed import clock. The caller supplies its own current operation
    /// clock. Only this original committed carrier/opaque ACK can enter here.
    pub fn assert_pins_current(&self,ack:&LocalImagePinAck)->Result<()> {
        let state=self.borrow()?;
        ensure!(state.phase==Phase::Committed&&state.ack.as_ref()==Some(&ack.data)&&state.gc.is_none(),"image commit ACK absent/unknown");
        let retained=self.store.retention()?;
        ensure!(!retained.image_import_holds.contains_key(&state.hold.import_id)
            &&ack.data.pins.iter().all(|(name,reference)|retained.pins.get(name)==Some(reference)),"original image pins changed");
        Ok(())
    }
    pub fn assert_pins_current_for_import(&self,ack:&LocalImagePinAck)->Result<()> {
        let mut state=self.borrow()?;
        ensure!(state.phase==Phase::Committed&&state.ack.as_ref()==Some(&ack.data),"original image pin ACK absent/unknown");
        let result=(|| {
            self.acquire(&mut state)?;let retained=self.read_retention()?;
            ensure!(!retained.image_import_holds.contains_key(&state.hold.import_id)
                &&ack.data.pins.iter().all(|(name,reference)|retained.pins.get(name)==Some(reference)),"original image pins changed");
            self.check()
        })();
        self.finish(&mut state,result)
    }
    pub fn objects_after_commit(&self,ack:&LocalImagePinAck)->Result<Vec<ObjectRef>> {
        self.assert_pins_current(ack)?;
        Ok(self.borrow()?.graph.as_ref().context("original verified image graph absent")?.clone())
    }
    pub fn objects_for_import(&self,ack:&LocalImagePinAck)->Result<Vec<ObjectRef>> {
        self.assert_pins_current_for_import(ack)?;
        self.check()?;
        let graph=self.borrow()?.graph.as_ref().context("original verified image graph absent")?.clone();
        self.check()?;Ok(graph)
    }
    fn commit_state(&self,state:&mut State,mut retained:RetentionState)->Result<()> {
        self.check()?;
        retained.generation=retained.generation.checked_add(1).context("image retention generation overflow")?;
        let checksum=ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&retained)?).digest;
        let bytes=canonical_json(&StateEnvelope {state:retained.clone(),checksum})?;
        ensure!(bytes.len()as u64<=MAX_RETENTION_BYTES,"image import retention state exceeds byte bound");
        state.retention_intent=Some(retained);
        let path=self.store.root.join("tmp").join(Uuid::new_v4().to_string());
        state.pending_path=Some(path.clone());self.check()?;state.pending_file=Some(create_private(&path)?);
        for chunk in bytes.chunks(64*1024){self.check()?;state.pending_file.as_mut().unwrap().write_all(chunk)?;}
        self.check()?;sync_file(state.pending_file.as_ref().unwrap())?;state.stats.retention_file_syncs+=1;self.check()?;
        #[cfg(test)] self.point(state,TestPoint::BeforeStateRename)?;
        self.check()?;fs::rename(&path,self.store.root.join("retention.json"))?;
        #[cfg(test)] self.point(state,TestPoint::AfterStateRename)?;
        for directory in [self.store.root.clone(),self.store.root.join("tmp")] {
            self.check()?;state.io=Some(File::open(&directory)?);sync_file(state.io.as_ref().unwrap())?;
            state.stats.directory_syncs+=1;self.check()?;state.io.take();
        }
        state.pending_file.take();state.pending_path=None;self.check()
    }
}

fn object_bound(reference:&ObjectRef,manifest:bool)->Result<()> {
    reference.validate()?;
    ensure!(matches!(reference.kind,ObjectKind::Blob|ObjectKind::File|ObjectKind::Directory|ObjectKind::DirectoryShard),"object kind outside image import");
    let limit=if reference.kind==ObjectKind::Blob {if manifest {MANIFEST}else{CHUNK}}else{64*1024};
    ensure!(reference.size_bytes<=limit&&(!manifest||reference.kind==ObjectKind::Blob),"image object/manifest byte bound exceeded");
    Ok(())
}
impl ContentStore for LocalImageImport {
    fn put_blob(&self,bytes:&[u8])->Result<ObjectRef> {
        let reference=ObjectRef::from_bytes(ObjectKind::Blob,bytes);self.receive(&reference,bytes)?;Ok(reference)
    }
    fn put_file_node(&self,node:&FileNode)->Result<ObjectRef> {
        node.validate()?;let bytes=canonical_json(node)?;let reference=ObjectRef::from_bytes(ObjectKind::File,&bytes);
        self.receive(&reference,&bytes)?;Ok(reference)
    }
    fn read_object(&self,reference:&ObjectRef)->Result<Vec<u8>> {self.read_for_trait(reference)}
}
impl DirectoryStore for LocalImageImport {
    fn read(&self,reference:&ObjectRef)->Result<Vec<u8>> {self.read_for_trait(reference)}
    fn put(&self,kind:ObjectKind,bytes:&[u8])->Result<ObjectRef> {
        ensure!(matches!(kind,ObjectKind::Directory|ObjectKind::DirectoryShard),"image directory sink received another kind");
        let reference=ObjectRef::from_bytes(kind,bytes);self.receive(&reference,bytes)?;Ok(reference)
    }
}

#[cfg(test)]
#[derive(Clone,Copy,PartialEq,Eq)]
enum TestPoint {ObjectLinked,BeforeObjectSync,AfterObjectSync,BeforeStateRename,AfterStateRename,AfterPinCommit}
#[cfg(test)]
impl LocalImageImport {
    fn point(&self,state:&mut State,point:TestPoint)->Result<()> {
        if state.fail_at==Some(point){state.fail_at=None;bail!("injected original image IO failure");}Ok(())
    }
}
#[cfg(test)]
thread_local! { static ACTUAL_SYNCS:std::cell::Cell<u64>=const {std::cell::Cell::new(0)}; }
#[cfg(test)]
pub(super) fn record_actual_sync() {ACTUAL_SYNCS.with(|n|n.set(n.get()+1));}

#[cfg(test)]
#[path="image_import_tests.rs"]
mod tests;
