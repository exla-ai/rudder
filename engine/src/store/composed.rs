//! Bounded object DATA over one writable LocalStore and one held baked source.
//! No pin, complete closure, image, source, Root or execution authority is made.
//! Reads never repair/copy the backing or call LocalStore's unbounded GC lock.
use super::*;
use super::baked_image::{BakedImageCas, BakedImageRead};
use crate::{content::{ContentStore, FileNode}, directory::DirectoryStore};
use std::{ffi::CString, os::fd::{AsRawFd, FromRawFd}, os::unix::fs::FileExt as ReadAt,
    sync::{Arc, Mutex, MutexGuard, atomic::{AtomicBool, AtomicUsize, Ordering}}};

const CHUNK: u64 = 4 * 1024 * 1024;
const METADATA: u64 = 64 * 1024;
const MAX_READS: usize = 8;
const MAX_BYTES: usize = 32 * 1024 * 1024;

fn bound(reference: &ObjectRef) -> Result<()> {
    reference.validate()?;
    let maximum = match reference.kind {
        ObjectKind::Blob => CHUNK,
        ObjectKind::Directory | ObjectKind::DirectoryShard | ObjectKind::File => METADATA,
        _ => bail!("object outside composed content profile"),
    };
    ensure!(reference.size_bytes <= maximum && (reference.kind == ObjectKind::Blob || reference.size_bytes > 0),
        "composed object exceeds ordinary chunk/metadata bound"); Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity { dev: u64, ino: u64, mode: u32, uid: u32, gid: u32 }
fn identity(m: &fs::Metadata) -> Identity {
    Identity { dev:m.dev(), ino:m.ino(), mode:m.mode(), uid:m.uid(), gid:m.gid() }
}
fn directory(file: &File) -> Result<Identity> {
    let m=file.metadata()?;
    ensure!(m.is_dir() && m.uid()==unsafe{libc::geteuid()} && m.mode()&0o022==0,
        "composed writable directory is not private/current-owner data"); Ok(identity(&m))
}
fn lock_identity(file: &File) -> Result<Identity> {
    let m=file.metadata()?;
    ensure!(m.is_file() && m.uid()==unsafe{libc::geteuid()} && m.mode()&0o022==0 && m.nlink()==1 && m.len()==0,
        "composed original GC lock not regular/private"); Ok(identity(&m))
}
fn open_at(parent:&File,name:&str,dir:bool)->std::io::Result<File> {
    if name.is_empty() || name=="." || name==".." || name.contains(['/', '\0']) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,"invalid CAS child component"));
    }
    let name=CString::new(name).map_err(|e|std::io::Error::new(std::io::ErrorKind::InvalidInput,e))?;
    let fd=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_NOFOLLOW|libc::O_CLOEXEC
        |libc::O_NONBLOCK|if dir{libc::O_DIRECTORY}else{0})};
    if fd<0{return Err(std::io::Error::last_os_error());} Ok(unsafe{File::from_raw_fd(fd)})
}
fn directory_current(parent:&File,name:&str,held:&File,expected:&Identity)->Result<()> {
    ensure!(directory(held)?==*expected && directory(&open_at(parent,name,true)?)?==*expected,
        "composed original directory replaced/changed"); Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp { id:Identity, len:u64, links:u64, mtime:(i64,i64), ctime:(i64,i64) }
fn stamp(file:&File)->Result<Stamp> {
    let m=file.metadata()?;
    ensure!(m.is_file() && m.uid()==unsafe{libc::geteuid()} && m.mode()&0o7777==0o444 && m.nlink()==1,
        "composed local object is not immutable regular data");
    Ok(Stamp{id:identity(&m),len:m.len(),links:m.nlink(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})
}

/// Actual held local directory/lock FDs plus the actual baked source. Capture
/// grants DATA reading only; the enclosing owner still holds pins/epochs/GC
/// obligations. Capture's clock is retained but never renewed or reused as a
/// later read deadline: every operation receives its caller's original fence.
pub struct ComposedImageStore {
    local:LocalStore, baked:Arc<BakedImageCas>, capture_original:Arc<dyn ImageImportFence>,
    root:Option<File>, root_id:Option<Identity>, objects:Option<File>, objects_id:Option<Identity>,
    lock:Option<File>, lock_id:Option<Identity>, captured:bool, fenced:AtomicBool,
    active:AtomicUsize, bytes:AtomicUsize,
}
impl ComposedImageStore {
    pub fn capture_retaining(out:&mut Option<Arc<Self>>,local:LocalStore,baked:Arc<BakedImageCas>,
        original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"composed original capture output occupied");
        *out=Some(Arc::new(Self{local,baked,capture_original:original,root:None,root_id:None,objects:None,
            objects_id:None,lock:None,lock_id:None,captured:false,fenced:AtomicBool::new(false),active:AtomicUsize::new(0),bytes:AtomicUsize::new(0)}));
        let source=Arc::get_mut(out.as_mut().unwrap()).context("composed capture already aliased")?;
        let result=source.capture();if result.is_err(){source.fenced.store(true,Ordering::Release);}result
    }
    fn capture(&mut self)->Result<()> {
        let original=self.capture_original.clone();original.remaining()?;
        self.root=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(self.local.root())?);
        self.root_id=Some(directory(self.root.as_ref().unwrap())?);self.current_inner(&*original)?;
        self.objects=Some(open_at(self.root.as_ref().unwrap(),"objects",true)?);
        self.objects_id=Some(directory(self.objects.as_ref().unwrap())?);self.current_inner(&*original)?;
        self.lock=Some(open_at(self.root.as_ref().unwrap(),"lock",false)?);
        self.lock_id=Some(lock_identity(self.lock.as_ref().unwrap())?);self.current_inner(&*original)?;
        // The bounded getter checks actual successful baked capture. Its DATA
        // inventory is NOT adopted as writable-store retention or image policy.
        self.baked.inventory_data()?;original.remaining()?;self.captured=true;Ok(())
    }
    fn current_inner(&self,original:&dyn ImageImportFence)->Result<()> {
        original.remaining()?;
        let physical=self.physical_current();
        if physical.is_err(){self.fenced.store(true,Ordering::Release);}physical?;
        original.remaining()?;Ok(())
    }
    fn physical_current(&self)->Result<()> {
        ensure!(!self.fenced.load(Ordering::Acquire),"composed original local custody fenced");
        let root=self.root.as_ref().context("composed root capture incomplete")?;
        let named=fs::symlink_metadata(self.local.root())?;
        ensure!(!named.file_type().is_symlink() && Some(&directory(root)?)==self.root_id.as_ref()
            && Some(&identity(&named))==self.root_id.as_ref(),"composed original LocalStore root changed");
        if let(Some(file),Some(id))=(&self.objects,&self.objects_id){directory_current(root,"objects",file,id)?;}
        if let(Some(file),Some(id))=(&self.lock,&self.lock_id){
            ensure!(lock_identity(file)?==*id && lock_identity(&open_at(root,"lock",false)?)?==*id,"composed original GC inode changed");
        }
        Ok(())
    }
    fn current(&self,original:&dyn ImageImportFence)->Result<()> {
        ensure!(self.captured,"composed original capture incomplete");
        self.current_inner(original)
    }
    pub fn read_object_retaining(self:&Arc<Self>,out:&mut Option<ComposedRead>,reference:ObjectRef,
        original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"composed original read output occupied");
        *out=Some(ComposedRead::stage(self.clone(),reference,original));out.as_mut().unwrap().run()
    }
    /// Request-scoped read-only trait adapter. It owns the exact caller fence;
    /// no default clock, writer, path fallback or fresh pin is created.
    pub fn read_session(self:&Arc<Self>,original:Arc<dyn ImageImportFence>)->ComposedReadSession {
        ComposedReadSession{source:self.clone(),original,state:Mutex::new(SessionState{pending:None,failed:false})}
    }
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ComposedReadSource { WritableLocal, BakedImage }
struct Reservation {source:Arc<ComposedImageStore>,active:bool,bytes:usize}
impl Drop for Reservation {fn drop(&mut self){
    if self.active{self.source.active.fetch_sub(1,Ordering::AcqRel);}if self.bytes>0{self.source.bytes.fetch_sub(self.bytes,Ordering::AcqRel);}
}}
/// Stage outside catch/unwind. Every failure keeps actual descriptors, GC
/// acquisition, partial bytes and backing read. A timeout is never completion.
#[must_use="retain the original composed read on every outcome"]
pub struct ComposedRead {
    source:Arc<ComposedImageStore>,reference:ObjectRef,original:Arc<dyn ImageImportFence>,
    reservation:Option<Reservation>,gc:Option<File>,gc_locked:bool,shard:Option<File>,shard_id:Option<Identity>,
    file:Option<File>,file_stamp:Option<Stamp>,bytes:Vec<u8>,baked:Option<BakedImageRead>,
    selected:Option<ComposedReadSource>,attempted:bool,verified:bool,
    #[cfg(test)] cancel_after_first_chunk:Option<Arc<AtomicBool>>,
}
impl ComposedRead {
    fn stage(source:Arc<ComposedImageStore>,reference:ObjectRef,original:Arc<dyn ImageImportFence>)->Self {
        Self{source,reference,original,reservation:None,gc:None,gc_locked:false,shard:None,shard_id:None,file:None,
            file_stamp:None,bytes:Vec::new(),baked:None,selected:None,attempted:false,verified:false,#[cfg(test)]cancel_after_first_chunk:None}
    }
    pub fn reference_data(&self)->&ObjectRef{&self.reference}
    pub fn source_data(&self)->Option<ComposedReadSource>{self.selected}
    pub fn verified_bytes(&self)->Result<&[u8]> {
        ensure!(self.verified,"composed original bytes unverified/unknown");
        match self.selected{Some(ComposedReadSource::WritableLocal)=>Ok(&self.bytes),Some(ComposedReadSource::BakedImage)=>
            self.baked.as_ref().context("composed actual baked read absent")?.verified_bytes(),None=>bail!("composed read source absent")}
    }
    fn run(&mut self)->Result<()> {
        ensure!(!self.attempted,"composed original read cannot retry");self.attempted=true;
        self.original.remaining()?;bound(&self.reference)?;self.source.current(&*self.original)?;
        self.reservation=Some(Reservation{source:self.source.clone(),active:false,bytes:0});
        self.source.active.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<MAX_READS).then_some(n+1))
            .map_err(|_|anyhow::anyhow!("composed read owner capacity exhausted"))?;
        self.reservation.as_mut().unwrap().active=true;let amount=usize::try_from(self.reference.size_bytes)?;
        self.source.bytes.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|n.checked_add(amount).filter(|n|*n<=MAX_BYTES))
            .map_err(|_|anyhow::anyhow!("composed read byte capacity exhausted"))?;
        self.reservation.as_mut().unwrap().bytes=amount;
        if self.read_local()? {self.selected=Some(ComposedReadSource::WritableLocal);}
        else {
            // Only a known absent shard/leaf under the actual original shared
            // GC lock reaches here. No corruption/permission/custody error does.
            self.gc.take();self.gc_locked=false;self.selected=Some(ComposedReadSource::BakedImage);
            self.source.baked.read_object_retaining(&mut self.baked,self.reference.clone(),self.original.clone())?;
        }
        self.source.current(&*self.original)?;self.original.remaining()?;self.verified=true;
        // A successful immutable byte snapshot needs no further mutable CAS
        // lock. Failed/unknown calls retain it with the original carrier.
        self.gc.take();self.gc_locked=false;Ok(())
    }
    fn read_local(&mut self)->Result<bool> {
        self.source.current(&*self.original)?;
        self.gc=Some(open_at(self.source.root.as_ref().unwrap(),"lock",false)?);
        ensure!(Some(&lock_identity(self.gc.as_ref().unwrap())?)==self.source.lock_id.as_ref(),"composed GC open changed inode");
        loop {self.source.current(&*self.original)?;
            match FileExt::try_lock_shared(self.gc.as_ref().unwrap()) {
                Ok(())=>{self.gc_locked=true;break;},
                Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>std::thread::sleep(self.original.remaining()?.min(Duration::from_millis(1))),
                Err(e)=>return Err(e.into()),
            }
        }
        self.source.current(&*self.original)?;
        let shard=&self.reference.digest[..2];let leaf=&self.reference.digest[2..];
        self.shard=match open_at(self.source.objects.as_ref().unwrap(),shard,true) {
            Ok(file)=>Some(file),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{self.source.current(&*self.original)?;return Ok(false);},
            Err(e)=>return Err(e.into()),
        };
        self.shard_id=Some(directory(self.shard.as_ref().unwrap())?);
        self.shard_current()?;
        self.file=match open_at(self.shard.as_ref().unwrap(),leaf,false) {
            Ok(file)=>Some(file),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{self.shard_current()?;self.source.current(&*self.original)?;return Ok(false);},
            Err(e)=>return Err(e.into()),
        };
        self.file_stamp=Some(stamp(self.file.as_ref().unwrap())?);
        ensure!(self.file_stamp.as_ref().unwrap().len==self.reference.size_bytes,"composed local object length differs");
        ensure!(stamp(&open_at(self.shard.as_ref().unwrap(),leaf,false)?)?==*self.file_stamp.as_ref().unwrap(),"composed local name changed at open");
        self.bytes.try_reserve_exact(usize::try_from(self.reference.size_bytes)?)?;
        let mut buffer=vec![0;64*1024];let mut sha=Sha256::new();let mut offset=0u64;
        loop {self.original.remaining()?;let n=self.file.as_ref().unwrap().read_at(&mut buffer,offset)?;if n==0{break;}
            ensure!(offset+(n as u64)<=self.reference.size_bytes,"composed local object grew");
            sha.update(&buffer[..n]);self.bytes.extend_from_slice(&buffer[..n]);offset+=n as u64;
            #[cfg(test)] if let Some(flag)=&self.cancel_after_first_chunk{flag.store(true,Ordering::Release);}
        }
        ensure!(offset==self.reference.size_bytes && hex::encode(sha.finalize())==self.reference.digest,"composed local checksum differs");
        self.reference.verify(&self.bytes)?;references_from_bytes(self.reference.kind,&self.bytes)?;
        ensure!(stamp(self.file.as_ref().unwrap())?==*self.file_stamp.as_ref().unwrap()
            &&stamp(&open_at(self.shard.as_ref().unwrap(),leaf,false)?)?==*self.file_stamp.as_ref().unwrap(),"composed local inode/name changed during read");
        self.shard_current()?;self.source.current(&*self.original)?;Ok(true)
    }
    fn shard_current(&self)->Result<()> {
        self.original.remaining()?;
        directory_current(self.source.objects.as_ref().unwrap(),&self.reference.digest[..2],
            self.shard.as_ref().unwrap(),self.shard_id.as_ref().unwrap())
    }
}

struct SessionState {pending:Option<ComposedRead>,failed:bool}
/// Observation only: never a callback/thread/TLS/process or Root drain proof.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ComposedSessionState { Idle, Retained, Borrowed, Poisoned }
/// Read-only PagedDirectory/ContentStore adapter, with an actual bounded
/// retained slot for errors/panics that their Vec-returning traits cannot own.
pub struct ComposedReadSession {source:Arc<ComposedImageStore>,original:Arc<dyn ImageImportFence>,state:Mutex<SessionState>}
impl ComposedReadSession {
    pub fn state_data(&self)->ComposedSessionState {
        match self.state.try_lock(){
            Ok(state) if state.pending.is_none()&&!state.failed=>ComposedSessionState::Idle,
            Ok(_)=>ComposedSessionState::Retained,
            Err(std::sync::TryLockError::WouldBlock)=>ComposedSessionState::Borrowed,
            Err(std::sync::TryLockError::Poisoned(_))=>ComposedSessionState::Poisoned,
        }
    }
    fn lock(&self)->Result<MutexGuard<'_,SessionState>> {
        loop {self.original.remaining()?;match self.state.try_lock(){
            Ok(guard)=>return Ok(guard),Err(std::sync::TryLockError::WouldBlock)=>std::thread::sleep(self.original.remaining()?.min(Duration::from_millis(1))),
            Err(_)=>bail!("composed read session poisoned; original read retained"),
        }}
    }
    fn read(&self,reference:&ObjectRef)->Result<Vec<u8>> {
        let mut state=self.lock()?;ensure!(!state.failed && state.pending.is_none(),"composed session retains an unsettled original read");
        let result=(||{self.source.read_object_retaining(&mut state.pending,reference.clone(),self.original.clone())?;
            let bytes=state.pending.as_ref().unwrap().verified_bytes()?.to_vec();self.original.remaining()?;Ok(bytes)})();
        match result{Ok(bytes)=>{state.pending.take();Ok(bytes)},Err(e)=>{state.failed=true;Err(e)}}
    }
    /// Pure nonblocking custody transfer after the actual caller/worker has
    /// returned/unwound. Even an expired/poisoned session preserves its carrier;
    /// this is not a join, cleanup receipt, retry permission or successful read.
    pub fn take_retained_read(&self,out:&mut Option<ComposedRead>)->Result<()> {
        ensure!(out.is_none(),"composed recovery output occupied");
        let mut state=match self.state.try_lock(){Ok(guard)=>guard,Err(std::sync::TryLockError::Poisoned(e))=>e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock)=>bail!("composed actual read still borrowed")};
        ensure!(state.pending.is_some(),"composed retained read absent");state.failed=true;*out=state.pending.take();Ok(())
    }
}
impl DirectoryStore for ComposedReadSession {
    fn read(&self,object:&ObjectRef)->Result<Vec<u8>>{ComposedReadSession::read(self,object)}
    fn put(&self,_:ObjectKind,_:&[u8])->Result<ObjectRef>{bail!("composed reader writes require the separate actual writable staging owner")}
}
impl ContentStore for ComposedReadSession {
    fn put_blob(&self,_:&[u8])->Result<ObjectRef>{bail!("composed reader cannot write backing/local content")}
    fn put_file_node(&self,_:&FileNode)->Result<ObjectRef>{bail!("composed reader cannot write backing/local content")}
    fn read_object(&self,reference:&ObjectRef)->Result<Vec<u8>>{self.read(reference)}
    fn read_blob_range(&self,reference:&ObjectRef,offset:u64,length:usize)->Result<Vec<u8>> {
        ensure!(reference.kind==ObjectKind::Blob && length<=crate::content::MAX_RANGE_BYTES,"invalid composed raw range");
        let bytes=self.read(reference)?;self.original.remaining()?;
        let start=usize::try_from(offset.min(reference.size_bytes))?;let count=(bytes.len()-start).min(length);
        let result=bytes[start..start+count].to_vec();self.original.remaining()?;Ok(result)
    }
    fn copy_blob_to(&self,reference:&ObjectRef,writer:&mut dyn Write)->Result<()> {
        ensure!(reference.kind==ObjectKind::Blob,"composed copy requires Blob");let bytes=self.read(reference)?;
        // The caller owns this external writer; synchronous kernel writes are
        // not preemptible and their effects are not a composed read receipt.
        for chunk in bytes.chunks(64*1024){self.original.remaining()?;writer.write_all(chunk)?;self.original.remaining()?;}Ok(())
    }
}

#[cfg(test)]
#[path="composed_tests.rs"]
mod tests;
