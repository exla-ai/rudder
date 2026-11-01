//! OFF-App, original fork custody and bounded removal of its Git alternate.
//! The native caller owns genuine helper/worker/ODB-reader quiescence. This
//! module proves local bytes/durability, never backend drain or acceptance.
use super::*;
use gix::{ObjectId, objs::{Find as _, Kind}, odb::HeaderExt as _};
use jj_lib::{object_id::ObjectId as _, repo::{RepoLoader, StoreFactories}};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use std::{collections::BTreeSet, ffi::{CString, OsString}, io,
    os::{fd::{AsRawFd, FromRawFd}, unix::{ffi::OsStrExt, fs::{FileExt, MetadataExt}}},
    sync::{Arc, atomic::{AtomicBool, Ordering}}};

/// Cumulative work, including explicit reconciliation, remains charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForkLocalizationLimits {
    pub max_files: u64, pub max_file_bytes: u64, pub max_copy_bytes: u64,
    pub max_io_bytes: u64, pub max_objects: u64, pub max_object_bytes: u64,
    pub max_decoded_bytes: u64, pub max_edges: u64, pub max_operations: u64,
}
impl Default for ForkLocalizationLimits {
    fn default() -> Self { Self { max_files: 32_768, max_file_bytes: 128 << 20,
        max_copy_bytes: 384 << 20, max_io_bytes: 3 << 30, max_objects: 100_000,
        max_object_bytes: 32 << 20, max_decoded_bytes: 384 << 20,
        max_edges: 1_000_000, max_operations: 32_768 } }
}
impl ForkLocalizationLimits {
    fn validate(self) -> Result<()> {
        ensure!(self.max_files > 0 && self.max_files <= 65_536
            && self.max_file_bytes > 0 && self.max_file_bytes <= 256 << 20
            && self.max_copy_bytes > 0 && self.max_copy_bytes <= 1 << 30
            && self.max_io_bytes > 0 && self.max_io_bytes <= 8 << 30
            && self.max_objects > 0 && self.max_objects <= 1_000_000
            && self.max_object_bytes > 0 && self.max_object_bytes <= 128 << 20
            && self.max_decoded_bytes > 0 && self.max_decoded_bytes <= 1 << 30
            && self.max_edges > 0 && self.max_edges <= 4_000_000
            && self.max_operations > 0 && self.max_operations <= 65_536,
            "fork localization limits exceed bounded policy"); Ok(())
    }
}
#[derive(Default)]
struct Work { io: u64, copied: u64, files: u64, objects: u64, decoded: u64, edges: u64 }
fn tick(budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire){budget.cancel();anyhow::bail!("fork operation cancelled");}
    budget.remaining()?;
    if cancelled.load(Ordering::Acquire){budget.cancel();anyhow::bail!("fork operation cancelled");}Ok(())
}
fn phase(budget: &FinalizationBudget, cancelled: &AtomicBool, binding: &AttemptBinding,
    check: &mut impl FnMut(&AttemptBinding) -> Result<()>) -> Result<()> {
    tick(budget, cancelled)?; check(binding)?; tick(budget, cancelled)
}
fn charge(work: &mut Work, amount: u64, limits: ForkLocalizationLimits) -> Result<()> {
    work.io = work.io.checked_add(amount).context("fork I/O counter overflow")?;
    ensure!(work.io <= limits.max_io_bytes, "fork I/O work exhausted"); Ok(())
}
fn c_name(name: &std::ffi::OsStr) -> Result<CString> {
    ensure!(!name.as_bytes().is_empty() && name != "." && name != ".."
        && !name.as_bytes().contains(&b'/'), "invalid single fork component");
    Ok(CString::new(name.as_bytes())?)
}
fn identity(file: &File) -> Result<(u64, u64)> { let m=file.metadata()?; Ok((m.dev(),m.ino())) }
fn fd_path(file: &File) -> PathBuf { PathBuf::from(format!("/proc/self/fd/{}",file.as_raw_fd())) }
struct Dir { file: File, path: PathBuf, private: bool }
impl Dir {
    fn open(path: &Path, private: bool) -> Result<Arc<Self>> {
        ensure!(path.is_absolute() && path.canonicalize()? == path, "fork directory must be canonical");
        let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_DIRECTORY|libc::O_CLOEXEC).open(path)?;
        let value=Arc::new(Self {file,path:path.into(),private}); value.check()?; Ok(value)
    }
    fn child(self: &Arc<Self>, name: &std::ffi::OsStr, private: bool) -> Result<Arc<Self>> {
        let name_c=c_name(name)?;
        let fd=unsafe{libc::openat(self.file.as_raw_fd(),name_c.as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC)};
        ensure!(fd>=0,"open original fork directory: {}",io::Error::last_os_error());
        let value=Arc::new(Self {file:unsafe{File::from_raw_fd(fd)},path:self.path.join(name),private}); value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> {
        let held=self.file.metadata()?; let named=fs::symlink_metadata(&self.path)?;
        ensure!(held.is_dir() && named.is_dir() && (held.dev(),held.ino())==(named.dev(),named.ino()),
            "original fork directory identity changed");
        if self.private { ensure!(held.uid()==unsafe{libc::geteuid()} && held.mode()&0o022==0,
            "private fork directory custody/mode differs"); } Ok(())
    }
    fn open_file(self: &Arc<Self>, name: &std::ffi::OsStr) -> Result<HeldFile> {
        let c=c_name(name)?;
        let fd=unsafe{libc::openat(self.file.as_raw_fd(),c.as_ptr(),libc::O_RDONLY|libc::O_NONBLOCK|libc::O_NOFOLLOW|libc::O_CLOEXEC)};
        ensure!(fd>=0,"open original fork file: {}",io::Error::last_os_error());
        let value=HeldFile {file:unsafe{File::from_raw_fd(fd)},parent:Arc::clone(self),name:name.into()};
        let metadata=value.file.metadata()?;
        ensure!(metadata.is_file(),"fork node is not regular");
        if self.private {ensure!(metadata.uid()==unsafe{libc::geteuid()} && metadata.mode()&0o022==0,
            "private fork file custody/mode differs");}
        value.check()?; Ok(value)
    }
    fn absent(&self, name: &std::ffi::OsStr) -> Result<bool> {
        let c=c_name(name)?;let mut stat=std::mem::MaybeUninit::<libc::stat>::uninit();
        let result=unsafe{libc::fstatat(self.file.as_raw_fd(),c.as_ptr(),stat.as_mut_ptr(),libc::AT_SYMLINK_NOFOLLOW)};
        if result==0{return Ok(false)}let error=io::Error::last_os_error();
        if error.kind()==io::ErrorKind::NotFound{Ok(true)}else{Err(error.into())}
    }
}
struct HeldFile { file: File, parent: Arc<Dir>, name: OsString }
impl HeldFile {
    fn check(&self) -> Result<()> {
        self.parent.check()?;let c=c_name(&self.name)?;let mut raw=std::mem::MaybeUninit::<libc::stat>::uninit();
        ensure!(unsafe{libc::fstatat(self.parent.file.as_raw_fd(),c.as_ptr(),raw.as_mut_ptr(),libc::AT_SYMLINK_NOFOLLOW)}==0,
            "original fork file name disappeared");let raw=unsafe{raw.assume_init()};let meta=self.file.metadata()?;
        ensure!(meta.is_file() && raw.st_mode&libc::S_IFMT==libc::S_IFREG
            && (meta.dev(),meta.ino())==(raw.st_dev as u64,raw.st_ino as u64),"original fork file identity changed");Ok(())
    }
}
fn descriptor(file: &File, limits: ForkLocalizationLimits, work: &mut Work,
    budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<SeedFile> {
    tick(budget,cancelled)?;let before=file.metadata()?;
    ensure!(before.is_file() && before.len()<=limits.max_file_bytes,"fork file byte bound");
    let mut offset=0;let mut sha=Sha256::new();let mut buf=[0u8;65536];
    while offset<before.len(){tick(budget,cancelled)?;let amount=(before.len()-offset).min(buf.len() as u64) as usize;
        charge(work,amount as u64,limits)?;let n=file.read_at(&mut buf[..amount],offset)?;
        ensure!(n>0,"fork file shortened during read");sha.update(&buf[..n]);offset+=n as u64;}
    let after=file.metadata()?;
    ensure!((before.dev(),before.ino(),before.len(),before.mtime(),before.mtime_nsec(),before.ctime(),before.ctime_nsec())
        ==(after.dev(),after.ino(),after.len(),after.mtime(),after.mtime_nsec(),after.ctime(),after.ctime_nsec()),"fork file changed during read");
    Ok(SeedFile{sha256:hex::encode(sha.finalize()),size_bytes:offset})
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyStage { Writing, FileSynced, RenameIntent, Installed }
enum CopySource { File(HeldFile), Bytes(Vec<u8>) }
struct CopyJob { source: CopySource, output: File, parent: Arc<Dir>, temporary: OsString,
    final_name: OsString, expected: SeedFile, mode: u32, stage: CopyStage }
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault { AfterRename, AfterUnlink, BeforeDirectorySync, #[cfg(test)] FenceAfterAck }
fn inject(fault: &mut Option<Fault>, at: Fault) -> Result<()> {
    if *fault==Some(at){*fault=None;anyhow::bail!("injected unknown fork effect");}Ok(())
}
fn same_named(parent: &Arc<Dir>, name: &std::ffi::OsStr, file: &File) -> Result<bool> {
    if parent.absent(name)? {return Ok(false)} Ok(identity(&parent.open_file(name)?.file)?==identity(file)?)
}
fn drive_copy(job: &mut CopyJob, reconcile: bool, limits: ForkLocalizationLimits, work: &mut Work,
    budget: &FinalizationBudget, cancelled: &AtomicBool, binding: &AttemptBinding,
    check: &mut impl FnMut(&AttemptBinding)->Result<()>, fault: &mut Option<Fault>) -> Result<()> {
    phase(budget,cancelled,binding,check)?; job.parent.check()?;
    if reconcile && job.stage==CopyStage::RenameIntent {
        if same_named(&job.parent,&job.final_name,&job.output)? && job.parent.absent(&job.temporary)? {
            job.stage=CopyStage::Installed;
        }else{ensure!(same_named(&job.parent,&job.temporary,&job.output)? && job.parent.absent(&job.final_name)?,
            "unknown rename does not match its original temporary/final inode");job.stage=CopyStage::FileSynced;}
    }
    if job.stage==CopyStage::Writing {
        ensure!(same_named(&job.parent,&job.temporary,&job.output)? && job.parent.absent(&job.final_name)?,"unpublished copy identity differs");
        // Explicit reconciliation only: restarting this SAME unpublished inode
        // cannot truncate an installed object or any pre-existing history.
        if reconcile {phase(budget,cancelled,binding,check)?;job.output.set_len(0)?;}
        let mut offset=0;let mut hash=Sha256::new();let mut buf=[0u8;65536];
        while offset<job.expected.size_bytes {phase(budget,cancelled,binding,check)?;
            let count=(job.expected.size_bytes-offset).min(buf.len() as u64) as usize;
            charge(work,2*count as u64,limits)?;
            match &job.source {CopySource::File(source)=>source.file.read_exact_at(&mut buf[..count],offset)?,
                CopySource::Bytes(bytes)=>buf[..count].copy_from_slice(&bytes[offset as usize..offset as usize+count])};
            job.output.write_all_at(&buf[..count],offset)?;hash.update(&buf[..count]);offset+=count as u64;
        }
        ensure!(hex::encode(hash.finalize())==job.expected.sha256,"shared seed bytes changed during copy");
        ensure!(descriptor(&job.output,limits,work,budget,cancelled)?==job.expected,"private copied bytes differ");
        phase(budget,cancelled,binding,check)?;job.output.set_permissions(fs::Permissions::from_mode(job.mode))?;
        job.output.sync_all()?;job.stage=CopyStage::FileSynced;
    }
    if job.stage==CopyStage::FileSynced {
        phase(budget,cancelled,binding,check)?;job.stage=CopyStage::RenameIntent;
        let from=c_name(&job.temporary)?;let to=c_name(&job.final_name)?;
        let result=unsafe{libc::syscall(libc::SYS_renameat2,job.parent.file.as_raw_fd(),from.as_ptr(),
            job.parent.file.as_raw_fd(),to.as_ptr(),libc::RENAME_NOREPLACE)};
        ensure!(result==0,"private no-replace install uncertain: {}",io::Error::last_os_error());
        inject(fault,Fault::AfterRename)?;job.stage=CopyStage::Installed;
    }
    ensure!(job.stage==CopyStage::Installed && same_named(&job.parent,&job.final_name,&job.output)?,"installed object identity differs");
    phase(budget,cancelled,binding,check)?;inject(fault,Fault::BeforeDirectorySync)?;
    job.parent.file.sync_all()?;Ok(())
}
struct SnapshotFile { held: HeldFile, descriptor: SeedFile, mode: u32 }
struct Snapshot { files: BTreeMap<PathBuf, SnapshotFile> }
impl Snapshot {
    fn stamps(&self) -> Result<BTreeMap<PathBuf,(SeedFile,u32,(u64,u64))>> {
        self.files.iter().map(|(path,item)|Ok((path.clone(),(item.descriptor.clone(),item.mode,identity(&item.held.file)?)))).collect()
    }
    fn same_bytes_modes(&self, other: &Self) -> Result<bool> {
        if self.files.len()!=other.files.len(){return Ok(false)}
        for (path,a) in &self.files {
            let Some(b)=other.files.get(path) else{return Ok(false)};
            if a.descriptor!=b.descriptor || a.mode!=b.mode || identity(&a.held.file)?!=identity(&b.held.file)? {return Ok(false)}
        }Ok(true)
    }
}
struct Source { metadata: Arc<Dir>, objects: Arc<Dir> }
struct Private { parent: Arc<Dir>, name: OsString, dirs: BTreeMap<PathBuf,Arc<Dir>> }
impl Private {
    fn root(&self) -> Result<&Arc<Dir>> {self.dirs.get(Path::new("")).context("original fork root not yet captured")}
    fn check(&self) -> Result<()> {
        self.parent.check()?;let named=self.parent.child(&self.name,true)?;
        ensure!(identity(&named.file)?==identity(&self.root()?.file)?,"original root name changed");
        for dir in self.dirs.values(){dir.check()?;}Ok(())
    }
    fn dir(&mut self, rel: &Path, create: bool, budget: &FinalizationBudget,cancelled:&AtomicBool) -> Result<Arc<Dir>> {
        tick(budget,cancelled)?;
        ensure!(rel.components().count()<=64 && self.dirs.len()<=MAX_TEMPLATE_FILES,"fork directory depth/count bound");
        if let Some(dir)=self.dirs.get(rel){dir.check()?;return Ok(Arc::clone(dir))}
        let mut current=PathBuf::new();let mut parent=Arc::clone(self.root()?);
        for part in rel.components(){tick(budget,cancelled)?;let Component::Normal(name)=part else{anyhow::bail!("invalid fork directory component")};current.push(name);
            if let Some(known)=self.dirs.get(&current){parent=Arc::clone(known);continue}
            if parent.absent(name)? {ensure!(create,"required original directory absent");tick(budget,cancelled)?;
                let name_c=c_name(name)?;
                ensure!(unsafe{libc::mkdirat(parent.file.as_raw_fd(),name_c.as_ptr(),0o700)}==0,"create private fork directory failed");
                // A failed sync leaves the original root/parent and created
                // name held; no success proof or fresh-path reopen follows.
                let opened=parent.child(name,true)?;self.dirs.insert(current.clone(),Arc::clone(&opened));
                parent.file.sync_all()?;parent=opened;
            }else{let opened=parent.child(name,true)?;self.dirs.insert(current.clone(),Arc::clone(&opened));parent=opened;}
        }Ok(parent)
    }
    fn scan(&mut self, rel: &Path, skip: &[&Path], limits: ForkLocalizationLimits, work: &mut Work,
        budget:&FinalizationBudget,cancelled:&AtomicBool) -> Result<Snapshot> {
        fn visit(private:&mut Private,rel:&Path,skip:&[&Path],out:&mut Snapshot,limits:ForkLocalizationLimits,
            work:&mut Work,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
            tick(budget,cancelled)?;let dir=private.dir(rel,false,budget,cancelled)?;
            for entry in fs::read_dir(fd_path(&dir.file))? {tick(budget,cancelled)?;let entry=entry?;let name=entry.file_name();c_name(&name)?;
                let child=rel.join(&name);if skip.iter().any(|p|child.as_path()==*p||child.starts_with(*p)){continue}
                let kind=entry.file_type()?;
                if kind.is_dir(){visit(private,&child,skip,out,limits,work,budget,cancelled)?;}
                else{ensure!(kind.is_file() && (out.files.len() as u64)<limits.max_files,"unsupported/oversized private metadata inventory");
                    let held=dir.open_file(&name)?;let descriptor=descriptor(&held.file,limits,work,budget,cancelled)?;
                    let mode=held.file.metadata()?.mode()&0o777;out.files.insert(child,SnapshotFile{held,descriptor,mode});}
            }Ok(())
        }
        let mut value=Snapshot{files:BTreeMap::new()};visit(self,rel,skip,&mut value,limits,work,budget,cancelled)?;Ok(value)
    }
}

// Positive one-shot custody, born only in the original retaining factory.
// This is not a bool inferred from a missing file or pending/alternates slot.
struct NeverAttemptedAlternateInstall { _private: () }
struct OriginalAlternateInstallIntent { _permit: NeverAttemptedAlternateInstall }

/// Original, non-Serde carrier. Stage this BEFORE creation and retain on every
/// error/panic. No API adopts an old directory or reconstructs this from DATA.
#[must_use="retain original fork owner and partial effects through settlement"]
pub struct PrivateMetadataForkOwner {
    private: Option<Private>, source: Option<Source>, require_readonly_objects: bool, manifest: Arc<MetadataTemplate>,
    binding: AttemptBinding, seed_manifest: ObjectRef, shared_path: PathBuf,
    alternates: Option<HeldFile>, ready: bool, pending: Option<CopyJob>,
    alternate_install_permit: Option<NeverAttemptedAlternateInstall>,
    alternate_install_intent: Option<OriginalAlternateInstallIntent>,
    never_linked_release: Option<never_linked::NeverLinkedRequest>, never_linked_released: bool,
    localization: Option<(FinalizationBudget,ForkLocalizationLimits)>, history: Option<BTreeMap<PathBuf,(SeedFile,u32,(u64,u64))>>, failed: bool,
    shared_index: usize, verifier: PathBuf, detach_intent: bool, detached: bool,
    work: Work, reconciles: u8, fault: Option<Fault>, localized: bool, cow:Option<cow::CowMount>,
}
impl PrivateMetadataForkOwner {
    pub fn binding(&self)->&AttemptBinding{&self.binding}
    pub fn seed(&self)->&ObjectRef{&self.binding.seed}
    pub fn original_shared_objects_root(&self)->&Path{&self.shared_path}
    pub fn is_ready(&self)->bool{self.ready && !self.failed && !self.localized && self.localization.is_none() && self.never_linked_release.is_none() && !self.never_linked_released}
    fn private(&mut self)->Result<&mut Private>{self.private.as_mut().context("original private custody transferred to localized proof")}
    fn scan(&mut self,rel:&Path,skip:&[&Path],limits:ForkLocalizationLimits,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<Snapshot>{
        self.private.as_mut().context("original private custody absent")?.scan(rel,skip,limits,&mut self.work,budget,cancelled)
    }
    fn start_copy(&mut self, source:CopySource, rel:&Path, expected:SeedFile, mode:u32,
        limits:ForkLocalizationLimits,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        if rel==Path::new(".git/objects/info/alternates") {
            ensure!(self.alternate_install_intent.is_some() && self.alternate_install_permit.is_none()
                && self.never_linked_release.is_none() && !self.never_linked_released,
                "original one-shot alternate install intent must precede every effect");
        }
        ensure!(self.pending.is_none(),"previous copy remains unresolved");
        ensure!(expected.size_bytes<=limits.max_file_bytes && self.work.files<limits.max_files
            && expected.size_bytes<=limits.max_copy_bytes.saturating_sub(self.work.copied),"fork copy admission exhausted");
        let parent=self.private()?.dir(rel.parent().context("copy parent absent")?,true,budget,cancelled)?;
        let final_name=rel.file_name().context("copy filename absent")?.to_os_string();
        ensure!(parent.absent(&final_name)?,"private install would replace existing history");
        let temporary=OsString::from(format!(".rudder-copy-{}",uuid::Uuid::new_v4()));let name=c_name(&temporary)?;
        tick(budget,cancelled)?;
        let fd=unsafe{libc::openat(parent.file.as_raw_fd(),name.as_ptr(),libc::O_RDWR|libc::O_CREAT|libc::O_EXCL|libc::O_NOFOLLOW|libc::O_CLOEXEC,0o600)};
        ensure!(fd>=0,"create owned temporary failed");
        self.pending=Some(CopyJob{source,output:unsafe{File::from_raw_fd(fd)},parent,temporary,final_name,expected:expected.clone(),mode,stage:CopyStage::Writing});
        self.work.files+=1;self.work.copied+=expected.size_bytes;Ok(())
    }
    fn finish_copy(&mut self,reconcile:bool,limits:ForkLocalizationLimits,budget:&FinalizationBudget,cancelled:&AtomicBool,
        check:&mut impl FnMut(&AttemptBinding)->Result<()>)->Result<HeldFile> {
        drive_copy(self.pending.as_mut().context("original pending copy absent")?,reconcile,limits,&mut self.work,
            budget,cancelled,&self.binding,check,&mut self.fault)?;
        let job=self.pending.take().unwrap();Ok(HeldFile{file:job.output,parent:job.parent,name:job.final_name})
    }
    fn begin_original_alternate_install(&mut self)->Result<()> {
        ensure!(self.alternate_install_intent.is_none() && self.never_linked_release.is_none()
            && !self.never_linked_released,"original alternate install permit is unavailable");
        let permit=self.alternate_install_permit.take().context("original alternate install permit already consumed")?;
        self.alternate_install_intent=Some(OriginalAlternateInstallIntent{_permit:permit});
        Ok(())
    }
    fn inline(&mut self,rel:&str,bytes:Vec<u8>,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<HeldFile> {
        let descriptor=SeedFile{sha256:hex::encode(Sha256::digest(&bytes)),size_bytes:bytes.len() as u64};
        let limits=ForkLocalizationLimits::default();self.start_copy(CopySource::Bytes(bytes),Path::new(rel),descriptor,0o600,limits,budget,cancelled)?;
        self.finish_copy(false,limits,budget,cancelled,&mut |_|Ok(()))
    }
    fn original_alternate(&self)->Result<()> {
        let alt=self.alternates.as_ref().context("original alternates owner absent")?;alt.check()?;
        let expected=format!("{}\n",self.shared_path.to_str().context("shared root is not UTF8")?);
        let mut bytes=vec![0;expected.len()];ensure!(alt.file.metadata()?.len()==bytes.len() as u64,"original alternates length changed");
        alt.file.read_exact_at(&mut bytes,0)?;ensure!(bytes==expected.as_bytes(),"original alternates binding changed");Ok(())
    }
    /// First attempt only. All later continuation must be explicitly named
    /// reconciliation and use the SAME original budget and unchanged limits.
    pub fn localize_retaining(&mut self,out:&mut Option<LocalizedMetadataFork>,limits:ForkLocalizationLimits,
        original:&FinalizationBudget,cancelled:&AtomicBool,mut check_quiescent:impl FnMut(&AttemptBinding)->Result<()>)->Result<()> {
        ensure!(self.cow.is_none(),"V2 requires actual CoW checkpoint/Merkle localization, never byte-copy fallback");
        ensure!(self.localization.is_none(),"localization already attempted; reconcile original state explicitly");
        ensure!(self.is_ready() && out.is_none(),"fork is not ready or proof slot occupied");limits.validate()?;
        self.localization=Some((original.clone(),limits));self.failed=true;
        let result=self.localize(out,false,cancelled,&mut check_quiescent);if result.is_err(){self.failed=true;}result
    }
    pub fn reconcile_localization_retaining(&mut self,out:&mut Option<LocalizedMetadataFork>,limits:ForkLocalizationLimits,
        original:&FinalizationBudget,cancelled:&AtomicBool,mut check_quiescent:impl FnMut(&AttemptBinding)->Result<()>)->Result<()> {
        ensure!(self.failed && !self.localized && out.is_none() && self.reconciles<2,"no bounded original localization to reconcile");
        let (held,prior)=self.localization.as_ref().context("no original localization request")?;
        ensure!(*prior==limits && held.constraint()==original.constraint(),"reconciliation cannot replace limits/deadline");
        phase(held,cancelled,&self.binding,&mut check_quiescent)?;self.reconciles+=1;
        let result=self.localize(out,true,cancelled,&mut check_quiescent);if result.is_ok(){self.failed=false;}result
    }
    fn localize(&mut self,out:&mut Option<LocalizedMetadataFork>,reconcile:bool,cancelled:&AtomicBool,
        check:&mut impl FnMut(&AttemptBinding)->Result<()>)->Result<()> {
        let (budget,limits)=self.localization.as_ref().context("original localization absent")?.clone();
        let binding=self.binding.clone();
        let verifier=self.verifier.clone();
        phase(&budget,cancelled,&self.binding,check)?;self.private()?.check()?;
        if !self.detach_intent {self.original_alternate()?;}
        if let Some(source)=&self.source {source.objects.check()?;if self.require_readonly_objects {require_readonly_mount(&fd_path(&source.objects.file))?;}}
        if self.history.is_none(){
            let initial=self.scan(Path::new(""),&[Path::new(".git/objects"),&verifier],limits,&budget,cancelled)?;
            self.history=Some(initial.stamps()?);
        }
        if self.pending.is_some(){let _installed=self.finish_copy(reconcile,limits,&budget,cancelled,check)?;self.shared_index+=1;}
        let object_files:Vec<_>=self.manifest.objects.iter().map(|(p,d)|(p.clone(),d.clone())).collect();
        while self.shared_index<object_files.len(){phase(&budget,cancelled,&self.binding,check)?;
            let (rel,expected)=&object_files[self.shared_index];
            ensure!(rel!="info/alternates" && rel!="info/http-alternates","seed contains an external object dependency");
            let target=Path::new(".git/objects").join(relative_path(rel)?);
            let parent=self.private()?.dir(target.parent().unwrap(),true,&budget,cancelled)?;
            if !parent.absent(target.file_name().unwrap())? {
                let held=parent.open_file(target.file_name().unwrap())?;
                let source=source_file(&self.source.as_ref().context("original source custody absent")?.objects,Path::new(rel))?;
                ensure!(identity(&held.file)?!=identity(&source.file)?,"private object is a writable alias of the shared seed");
                ensure!(descriptor(&held.file,limits,&mut self.work,&budget,cancelled)?==*expected,"private/shared object collision differs");
            }else{
                let source=source_file(&self.source.as_ref().context("original shared descriptor absent")?.objects,Path::new(rel))?;
                ensure!(source.file.metadata()?.len()==expected.size_bytes,"shared object size changed");let mode=source.file.metadata()?.mode()&0o777;
                self.start_copy(CopySource::File(source),&target,expected.clone(),mode,limits,&budget,cancelled)?;
                let _installed=self.finish_copy(false,limits,&budget,cancelled,check)?;
            }self.shared_index+=1;
        }
        let before=self.scan(Path::new(""),&[Path::new(".git/objects"),&verifier],limits,&budget,cancelled)?;
        ensure!(self.history.as_ref()==Some(&before.stamps()?),"original quiescent history changed across localization attempts");
        let baseline_path=PathBuf::from(format!(".jj/repo/op_store/operations/{}",binding.baseline_operation.0));
        let baseline_operation=self.manifest.metadata.get(baseline_path.to_str().unwrap()).context("seed baseline operation descriptor absent")?.clone();
        ensure!(before.files.get(&baseline_path).is_some_and(|file|file.descriptor==baseline_operation),"original accepted operation bytes changed");
        validate_layout(self.private()?,&budget,cancelled)?;
        let objects=self.scan(Path::new(".git/objects"),&[Path::new(".git/objects/info/alternates")],limits,&budget,cancelled)?;
        let verify_dir=self.private()?.dir(&verifier,true,&budget,cancelled)?;
        for (rel,item) in &objects.files {
            phase(&budget,cancelled,&self.binding,check)?;
            let rel=rel.strip_prefix(".git/objects")?;
            ensure!(rel!=Path::new("info/http-alternates") && !rel.file_name().unwrap().as_bytes().starts_with(b".rudder-copy-"),"unknown object artifact/dependency");
            let target=verifier.join(rel);let parent=self.private()?.dir(target.parent().unwrap(),true,&budget,cancelled)?;
            let name=target.file_name().unwrap();
            if parent.absent(name)? {
                let old=c_name(&item.held.name)?;let new=c_name(name)?;
                ensure!(unsafe{libc::linkat(item.held.parent.file.as_raw_fd(),old.as_ptr(),parent.file.as_raw_fd(),new.as_ptr(),0)}==0,"private verification link uncertain");
            }
            ensure!(same_named(&parent,name,&item.held.file)?,"private verifier link changed object identity");
        }
        // The private verifier has NO alternates. Its hardlinks point ONLY to
        // independently copied/private objects, never to the shared seed.
        ensure!(verify_dir.absent(std::ffi::OsStr::new("alternates"))?,"verifier redirect");
        let kinds=verify_objects(&verify_dir,limits,&mut self.work,&budget,cancelled,&binding,check)?;
        {let (private,work)=(&mut self.private,&mut self.work);
            verify_jj_references(private.as_mut().context("original private custody absent")?,&binding,&before,&kinds,limits,work,&budget,cancelled,check)?;}
        let after=self.scan(Path::new(""),&[Path::new(".git/objects"),&verifier],limits,&budget,cancelled)?;
        ensure!(before.same_bytes_modes(&after)?,"private history/operation metadata changed during localization");
        for record in before.files.values().chain(objects.files.values()) {phase(&budget,cancelled,&self.binding,check)?;record.held.check()?;record.held.file.sync_all()?;}
        for dir in self.private()?.dirs.values(){phase(&budget,cancelled,&binding,check)?;dir.file.sync_all()?;}
        self.private()?.parent.file.sync_all()?;
        phase(&budget,cancelled,&self.binding,check)?;
        let info=self.private()?.dir(Path::new(".git/objects/info"),false,&budget,cancelled)?;
        if self.detach_intent && reconcile {
            let alt=self.alternates.as_ref().context("original alternate absent")?;
            if info.absent(std::ffi::OsStr::new("alternates"))? {
                ensure!(alt.file.metadata()?.nlink()==0,"absence is not the original unlink effect");self.detached=true;
            }else{self.original_alternate()?;ensure!(!self.detached,"detached alternate reappeared");}
        }
        if !self.detached {
            self.original_alternate()?;phase(&budget,cancelled,&self.binding,check)?;
            self.detach_intent=true;let name=CString::new("alternates").unwrap();
            ensure!(unsafe{libc::unlinkat(info.file.as_raw_fd(),name.as_ptr(),0)}==0,"original alternates unlink uncertain");
            inject(&mut self.fault,Fault::AfterUnlink)?;self.detached=true;
        }
        phase(&budget,cancelled,&self.binding,check)?;inject(&mut self.fault,Fault::BeforeDirectorySync)?;
        info.file.sync_all()?;
        for rel in [".git/objects",".git",""]{self.private()?.dir(Path::new(rel),false,&budget,cancelled)?.file.sync_all()?;}
        self.private()?.parent.file.sync_all()?;
        ensure!(info.absent(std::ffi::OsStr::new("alternates"))? && self.alternates.as_ref().unwrap().file.metadata()?.nlink()==0,"original unlink identity not settled");
        // No fallible postcheck may precede delivery of the actual ACK. Drop
        // every original shared FD; final proof retains only private custody.
        let private=self.private.take().unwrap();let alternates=self.alternates.take().unwrap();self.source.take();
        *out=Some(LocalizedMetadataFork{private,alternates,binding:self.binding.clone(),shared_path:self.shared_path.clone(),
            seed_manifest:self.seed_manifest.clone(),objects,limits,baseline_operation,verifier:self.verifier.clone(),kernel_readonly_objects:self.require_readonly_objects});self.localized=true;self.failed=false;
        #[cfg(test)]
        if self.fault==Some(Fault::FenceAfterAck){self.fault=None;budget.cancel();}
        out.as_ref().unwrap().check_current(&budget,cancelled)
    }
}
fn source_file(root:&Arc<Dir>,rel:&Path)->Result<HeldFile>{
    let mut parent=Arc::clone(root);let path=relative_path(rel.to_str().context("source relative path encoding")?)?;
    for part in path.parent().unwrap().components(){let Component::Normal(name)=part else{anyhow::bail!("source component")};parent=parent.child(name,false)?;}
    parent.open_file(path.file_name().unwrap())
}
fn validate_layout(private:&mut Private,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    tick(budget,cancelled)?;let git=private.dir(Path::new(".git"),false,budget,cancelled)?;
    ensure!(git.absent(std::ffi::OsStr::new("commondir"))? && git.absent(std::ffi::OsStr::new("shallow"))?,"redirected/shallow private Git store unsupported");
    let config=git.open_file(std::ffi::OsStr::new("config"))?;
    ensure!(config.file.metadata()?.len()<=1<<20,"private config exceeds bound");let mut bytes=vec![0;config.file.metadata()?.len() as usize];config.file.read_exact_at(&mut bytes,0)?;validate_private_git_config(&bytes)?;
    let store=private.dir(Path::new(".jj/repo/store"),false,budget,cancelled)?;
    let target=store.open_file(std::ffi::OsStr::new("git_target"))?;let mut bytes=[0u8;13];
    ensure!(target.file.metadata()?.len()==13,"private jj Git target differs");target.file.read_exact_at(&mut bytes,0)?;
    ensure!(&bytes==b"../../../.git","private jj Git target is redirected");Ok(())
}
fn verify_objects(dir:&Arc<Dir>,limits:ForkLocalizationLimits,work:&mut Work,budget:&FinalizationBudget,
    cancelled:&AtomicBool,binding:&AttemptBinding,check:&mut impl FnMut(&AttemptBinding)->Result<()>)
    ->Result<BTreeMap<ObjectId,Kind>> {
    phase(budget,cancelled,binding,check)?;dir.check()?;
    let db=gix::odb::at_opts(fd_path(&dir.file),Vec::<(ObjectId,ObjectId)>::new(),gix::odb::store::init::Options {
        object_hash:gix::hash::Kind::Sha1,use_multi_pack_index:false,..Default::default()})?;
    ensure!(db.store_ref().alternate_db_paths()?.is_empty(),"independent verifier has an alternate");
    let mut kinds=BTreeMap::new();let mut wanted=BTreeMap::new();let mut buffer=Vec::new();
    for id in db.store_ref().iter()? {
        phase(budget,cancelled,binding,check)?;let id=id?;
        if kinds.contains_key(&id){continue}
        work.objects=work.objects.checked_add(1).context("object work overflow")?;
        ensure!(work.objects<=limits.max_objects,"local Git object-count work exhausted");
        let header=db.header(id)?;
        ensure!(header.size()<=limits.max_object_bytes && header.size()<=limits.max_decoded_bytes.saturating_sub(work.decoded),
            "local Git decoded object byte bound");
        let object=db.try_find(id.as_ref(),&mut buffer).map_err(anyhow::Error::from_boxed)?
            .context("local Git object is missing")?;
        phase(budget,cancelled,binding,check)?;
        ensure!(object.kind==header.kind() && object.data.len() as u64==header.size()
            && gix::objs::compute_hash(gix::hash::Kind::Sha1,object.kind,object.data)?==id,"local Git raw identity differs");
        work.decoded+=header.size();kinds.insert(id,object.kind);
        let mut edge=|child:ObjectId,kind:Kind|->Result<()> {
            phase(budget,cancelled,binding,check)?;work.edges=work.edges.checked_add(1).context("edge work overflow")?;
            ensure!(work.edges<=limits.max_edges,"local Git edge work exhausted");
            if let Some(old)=wanted.insert(child,kind){ensure!(old==kind,"one Git object has conflicting required kinds");}Ok(())
        };
        match object.kind {
            Kind::Commit=>{let commit=gix::objs::CommitRef::from_bytes(object.data)?;
                commit.author()?;commit.committer()?;ensure!(commit.parents.len()<=1024,"excess Git parents");
                edge(commit.tree(),Kind::Tree)?;for parent in commit.parents(){edge(parent,Kind::Commit)?;}}
            Kind::Tree=>{let mut names=BTreeSet::new();let mut previous=None;
                for entry in gix::objs::TreeRefIter::from_bytes(object.data){let entry=entry?;let name:&[u8]=entry.filename.as_ref();
                    ensure!(!name.is_empty()&&name!=b"."&&name!=b".."&&!name.contains(&b'/')&&names.insert(name.to_vec())
                        &&previous.is_none_or(|old|old<entry),"malformed Git tree namespace/order");previous=Some(entry);
                    use gix::objs::tree::EntryKind;
                    let kind=match entry.mode.kind(){EntryKind::Tree=>Kind::Tree,
                        EntryKind::Blob|EntryKind::BlobExecutable|EntryKind::Link=>Kind::Blob,
                        EntryKind::Commit=>anyhow::bail!("Gitlink repositories are outside the localization profile")};
                    edge(entry.oid.to_owned(),kind)?;}}
            Kind::Tag=>{let tag=gix::objs::TagRef::from_bytes(object.data)?;tag.tagger()?;edge(tag.target(),tag.target_kind)?;}
            Kind::Blob=>(),
        }
    }
    for (id,kind) in wanted{ensure!(kinds.get(&id)==Some(&kind),"local Git closure has a missing/wrong-kind edge");}
    ensure!(db.store_ref().alternate_db_paths()?.is_empty(),"verifier acquired a redirected ODB");
    phase(budget,cancelled,binding,check)?;Ok(kinds)
}
fn verify_jj_references(private:&mut Private,binding:&AttemptBinding,snapshot:&Snapshot,kinds:&BTreeMap<ObjectId,Kind>,
    limits:ForkLocalizationLimits,work:&mut Work,budget:&FinalizationBudget,cancelled:&AtomicBool,
    check:&mut impl FnMut(&AttemptBinding)->Result<()>)->Result<()> {
    use jj_lib::{config::StackedConfig,settings::UserSettings};
    phase(budget,cancelled,binding,check)?;
    // In-memory jj defaults, not a per-user jj configuration lookup. The
    // library Git backend may inspect Git config while reading; no Git/jj
    // process, hook, filter, alias, helper or network transport is invoked.
    // Raw object verification above is an isolated, direct ODB read.
    let settings=UserSettings::from_config(StackedConfig::with_defaults())?;
    let repo_dir=private.dir(Path::new(".jj/repo"),false,budget,cancelled)?;
    let loader=RepoLoader::init_from_file_system(&settings,&repo_dir.path,&StoreFactories::default())?;
    let mut operations=BTreeSet::new();
    for path in snapshot.files.keys(){if let Some(parent)=path.parent(){
        if parent==Path::new(".jj/repo/op_store/operations") {
            let name=path.file_name().unwrap().to_str().context("operation name encoding")?;
            let id=crate::JjOperationId(name.into());id.validate()?;operations.insert(id.0);
        }
    }}
    operations.insert(binding.baseline_operation.0.clone());
    for id in pollster::block_on(loader.op_heads_store().get_op_heads())?{operations.insert(id.hex());}
    ensure!(operations.len() as u64<=limits.max_operations,"jj operation work bound");
    for id in operations {phase(budget,cancelled,binding,check)?;
        let operation=pollster::block_on(loader.load_operation(&jj_lib::op_store::OperationId::new(hex::decode(id)?)))?;
        for parent in operation.parent_ids(){phase(budget,cancelled,binding,check)?;
            work.edges=work.edges.checked_add(1).context("operation edge work overflow")?;
            ensure!(work.edges<=limits.max_edges,"operation edge work exhausted");
            let _actual_parent=pollster::block_on(loader.load_operation(parent))?;
        }
        let view=pollster::block_on(operation.view())?;
        for commit in view.all_referenced_commit_ids().chain(operation.all_referenced_commit_ids()){
            phase(budget,cancelled,binding,check)?;work.edges=work.edges.checked_add(1).context("jj reference work overflow")?;
            ensure!(work.edges<=limits.max_edges,"jj reference work exhausted");
            // jj's synthetic root has no Git object. Every real referenced
            // commit, including old operations/predecessors, must be local.
            ensure!(commit.as_bytes().len()==20,"jj operation has a non-SHA1 commit identity");
            if commit.as_bytes().iter().all(|byte|*byte==0){continue}
            let raw=ObjectId::try_from(commit.as_bytes())?;
            ensure!(kinds.get(&raw)==Some(&Kind::Commit),"retained jj operation references a nonlocal Git commit");
        }
    }
    let baseline=loader.store().get_commit(&jj_lib::backend::CommitId::new(hex::decode(&binding.baseline.commit.0)?))?;
    ensure!(crate::CommitIdentity::from_jj(&baseline)==binding.baseline,"accepted seed identity changed");
    phase(budget,cancelled,binding,check)?;
    // The loader, store and all temporary gix/jj reader handles drop HERE,
    // before detachment. They never enter the returned proof.
    Ok(())
}

/// Opaque retained acknowledgment of completed independent local storage.
/// This is NOT Clone/Serde, backend quiescence, a Journal fact, or acceptance.
#[must_use="retain actual local custody through the original Root retirement ACK"]
pub struct LocalizedMetadataFork {
    private:Private,alternates:HeldFile,binding:AttemptBinding,shared_path:PathBuf,
    seed_manifest:ObjectRef,objects:Snapshot,limits:ForkLocalizationLimits,baseline_operation:SeedFile,verifier:PathBuf,kernel_readonly_objects:bool,
}
impl LocalizedMetadataFork {
    pub fn binding(&self)->&AttemptBinding{&self.binding}
    pub fn seed(&self)->&ObjectRef{&self.binding.seed}
    pub fn seed_manifest(&self)->&ObjectRef{&self.seed_manifest}
    pub fn kernel_readonly_objects(&self)->bool{self.kernel_readonly_objects}
    /// Original name only. It is not opened or used to reconstruct authority.
    pub fn original_shared_objects_root(&self)->&Path{&self.shared_path}
    pub fn check_current(&self,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        tick(budget,cancelled)?;self.private.check()?;
        let mut private=Private{parent:Arc::clone(&self.private.parent),name:self.private.name.clone(),
            dirs:self.private.dirs.iter().map(|(p,d)|(p.clone(),Arc::clone(d))).collect()};
        validate_layout(&mut private,budget,cancelled)?;
        let info=private.dir(Path::new(".git/objects/info"),false,budget,cancelled)?;
        ensure!(info.absent(std::ffi::OsStr::new("alternates"))? && info.absent(std::ffi::OsStr::new("http-alternates"))?
            && self.alternates.file.metadata()?.nlink()==0,"localized fork acquired an alternate");
        let mut work=Work::default();
        for item in self.objects.files.values(){tick(budget,cancelled)?;item.held.check()?;
            ensure!(descriptor(&item.held.file,self.limits,&mut work,budget,cancelled)?==item.descriptor
                &&item.held.file.metadata()?.mode()&0o777==item.mode,"retained private object identity/bytes/mode changed");}
        let _current=private.scan(Path::new(".git/objects"),&[],self.limits,&mut work,budget,cancelled)?;
        let dir=private.dir(Path::new(".git/objects"),false,budget,cancelled)?;
        let kinds=verify_objects(&dir,self.limits,&mut work,budget,cancelled,&self.binding,&mut |_|Ok(()))?;
        let metadata=private.scan(Path::new(""),&[Path::new(".git/objects"),&self.verifier],self.limits,&mut work,budget,cancelled)?;
        let baseline_path=PathBuf::from(format!(".jj/repo/op_store/operations/{}",self.binding.baseline_operation.0));
        ensure!(metadata.files.get(&baseline_path).is_some_and(|file|file.descriptor==self.baseline_operation),"original accepted operation bytes changed");
        verify_jj_references(&mut private,&self.binding,&metadata,&kinds,self.limits,&mut work,budget,cancelled,&mut |_|Ok(()))?;
        ensure!(info.absent(std::ffi::OsStr::new("alternates"))? && info.absent(std::ffi::OsStr::new("http-alternates"))?,"alternate changed during local check");
        tick(budget,cancelled)
    }
}

/// Create the same sanitized metadata format as the legacy DATA helper, while
/// retaining original parent/root/source/alternates before fallible effects.
/// A partial preparation is deliberately not reopened or silently retried.
pub fn fork_private_metadata_retaining(out:&mut Option<PrivateMetadataForkOwner>,template:&VerifiedMetadataTemplate,
    destination:&Path,binding:AttemptBinding,activation:Option<&crate::LiveObservation>,
    original:&FinalizationBudget,cancelled:&AtomicBool)->Result<PrivateMetadataFork> {
    tick(original,cancelled)?;ensure!(template.profile()==SeedProfile::SmallV1,"V2 requires actual Root CoW fork receipt");
    ensure!(out.is_none(),"original fork owner slot occupied");binding.validate()?;
    ensure!(template.retained_seed()==Some(&binding.seed) && (template.require_readonly || cfg!(test)),
        "retaining fork requires actual retained read-only seed custody");
    ensure!(binding.baseline==template.manifest.baseline&&binding.baseline_operation==template.manifest.operation,"fork baseline differs");
    template.manifest.validate()?;
    let shared_path=template.object_mount.clone();
    let shared_text=shared_path.to_str().context("shared object mount is not UTF8")?.to_string();
    ensure!(shared_text.len()<4096&&!shared_text.contains(['\n','\r']),"invalid original alternate path");
    let metadata=Dir::open(&template.root.join("metadata"),false)?;
    tick(original,cancelled)?;let objects=Dir::open(&shared_path,false)?;
    if template.require_readonly{require_readonly_mount(&fd_path(&objects.file))?;}
    ensure!(destination.is_absolute() && !destination.starts_with(&template.root) && !destination.starts_with(&template.object_mount),
        "fork destination cannot overlap the original immutable seed");
    tick(original,cancelled)?;let parent=Dir::open(destination.parent().context("fork destination has no parent")?,true)?;
    ensure!(parent.file.metadata()?.mode()&0o777==0o700,"fork destination parent must be owner0700");
    let name=destination.file_name().context("fork destination has no name")?.to_os_string();c_name(&name)?;
    ensure!(parent.absent(&name)?,"retaining fork destination must be new");
    let limits=ForkLocalizationLimits::default();
    let private=Private{parent:Arc::clone(&parent),name:name.clone(),dirs:BTreeMap::new()};
    *out=Some(PrivateMetadataForkOwner{private:Some(private),source:Some(Source{metadata,objects}),require_readonly_objects:template.require_readonly,manifest:template.manifest.clone(),
        binding:binding.clone(),seed_manifest:template.identity.clone(),shared_path,alternates:None,ready:false,pending:None,
        alternate_install_permit:Some(NeverAttemptedAlternateInstall{_private:()}),alternate_install_intent:None,
        never_linked_release:None,never_linked_released:false,
        localization:None,history:None,failed:false,shared_index:0,verifier:PathBuf::from(format!(".rudder-local-odb-{}",uuid::Uuid::new_v4())),
        detach_intent:false,detached:false,work:Work::default(),reconciles:0,fault:None,localized:false,cow:None});
    let owner=out.as_mut().unwrap();
    let result=(||->Result<PrivateMetadataFork>{
        #[cfg(test)]
        never_linked::factory_fault(never_linked::FactoryFault::AfterOwnerStaged)?;
        tick(original,cancelled)?;let name_c=c_name(&name)?;
        ensure!(unsafe{libc::mkdirat(parent.file.as_raw_fd(),name_c.as_ptr(),0o700)}==0,"create original private root failed");
        let root=parent.child(&name,true)?;owner.private()?.dirs.insert(PathBuf::new(),root);parent.file.sync_all()?;
        let entries:Vec<_>=owner.manifest.metadata.iter().map(|(p,d)|(p.clone(),d.clone())).collect();
        let mut bytes=0;
        for (rel,expected) in &entries{tick(original,cancelled)?;
            let source=source_file(&owner.source.as_ref().unwrap().metadata,&relative_path(rel)?)?;
            ensure!(source.file.metadata()?.len()==expected.size_bytes,"seed metadata size changed");
            owner.start_copy(CopySource::File(source),&relative_path(rel)?,expected.clone(),0o600,limits,original,cancelled)?;
            let _copied=owner.finish_copy(false,limits,original,cancelled,&mut |_|Ok(()))?;bytes+=expected.size_bytes;
        }
        // Consume and retain intent BEFORE inline/start_copy can allocate,
        // write, rename or publish any bytes naming the shared object root.
        owner.begin_original_alternate_install()?;
        #[cfg(test)]
        never_linked::factory_fault(never_linked::FactoryFault::AfterAlternateIntent)?;
        owner.alternates=Some(owner.inline(".git/objects/info/alternates",format!("{}\n",shared_text).into_bytes(),original,cancelled)?);
        #[cfg(test)]
        never_linked::factory_fault(never_linked::FactoryFault::AfterAlternateInstalled)?;
        let working=owner.private()?.dir(Path::new(".jj/working_copy"),false,original,cancelled)?;
        let wc_type=working.open_file(std::ffi::OsStr::new("type"))?;
        ensure!(wc_type.file.metadata()?.len()<=128,"working-copy type bound");let mut wc=vec![0;wc_type.file.metadata()?.len() as usize];wc_type.file.read_exact_at(&mut wc,0)?;
        if wc==crate::working_copy::TYPE_NAME.as_bytes(){
            let observation=activation.context("Rudder metadata needs a fresh view observation")?;
            crate::working_copy::rebind_private_fork(&working.path,&binding,observation)?;
        }
        let receipt=PrivateMetadataFork{format_version:1,seed_manifest:template.identity.clone(),binding,
            metadata_files:entries.len() as u64,metadata_bytes:bytes,kernel_readonly_objects:template.require_readonly};
        let _ready=owner.inline("rudder-metadata-ready.json",canonical_json(&receipt)?,original,cancelled)?;
        tick(original,cancelled)?;owner.private()?.root()?.file.sync_all()?;parent.file.sync_all()?;
        owner.ready=true;Ok(receipt)
    })();
    if result.is_err(){owner.failed=true;}result
}

#[cfg(test)]
#[path="seed_fork_localization_tests.rs"]
mod tests;

#[path="seed_fork_never_linked.rs"]
mod never_linked;
pub use never_linked::NeverLinkedMetadataFork;

#[path="seed_cow_fork.rs"]
mod cow;
pub use cow::{CowMetadataForkQuiesced,fork_private_metadata_cow_retaining};
