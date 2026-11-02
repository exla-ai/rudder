//! Explicit backed retention, bounded original IO and conservative GC.
//! Storage DATA only: neither this receipt nor a deserialized record grants
//! Root execution, process readiness, project ownership or mount retirement.
use super::*;
use super::{backed_data::{self,BackedRetentionData,BackedHoldData,BackedPinData,BakedInputsBindingData},
    backed_inputs::{VerifiedBakedInputs,VerifiedBakedRead,Metadata,validate_edges},object_sets::backed_gc::RetainedObjectSetScan};
use std::{ffi::CString,os::fd::{AsRawFd,FromRawFd},sync::Arc};
const RETENTION:u64=16*1024*1024;
const REFS:usize=65_536;
const WORK:u64=128*1024*1024;
// Actual physical observations are tagged before any later clock callback.
// Clock cancellation is never classified by re-reading its remaining time.
#[derive(Debug)]struct ObservedObjectFailure(anyhow::Error);
impl std::fmt::Display for ObservedObjectFailure{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{write!(f,"{}",self.0)}}
impl std::error::Error for ObservedObjectFailure{fn source(&self)->Option<&(dyn std::error::Error+'static)>{Some(self.0.as_ref())}}
#[cfg(test)]thread_local!{static PHYSICAL_FAILURE_HOOK:std::cell::RefCell<Option<Box<dyn FnOnce()>>>=std::cell::RefCell::new(None);}
// Rc's marker makes the guard !Send/!Sync: cleanup must occur on the thread
// where this one-shot hook was installed, including unwinding test paths.
#[cfg(test)]pub(crate) struct PhysicalFailureHookGuard(std::marker::PhantomData<std::rc::Rc<()>>);
#[cfg(test)]impl Drop for PhysicalFailureHookGuard{fn drop(&mut self){PHYSICAL_FAILURE_HOOK.with(|slot|{slot.borrow_mut().take();});}}
#[cfg(test)]pub(crate) fn physical_failure_hook_for_test(hook:Box<dyn FnOnce()>)->PhysicalFailureHookGuard{
    PHYSICAL_FAILURE_HOOK.with(|slot|{let mut slot=slot.borrow_mut();assert!(slot.is_none(),"physical failure hook already installed on this test thread");*slot=Some(hook);});
    PhysicalFailureHookGuard(std::marker::PhantomData)
}
fn observed<T>(result:Result<T>)->Result<T>{result.map_err(|error|{
    // Failed syscalls such as EMFILE/EIO do not by themselves establish that
    // immutable data changed. Only an observed namespace absence/type/loop or
    // a completed in-memory validation failure can physically fence an owner.
    if error.chain().filter_map(|cause|cause.downcast_ref::<std::io::Error>()).any(|io|
        io.kind()!=std::io::ErrorKind::NotFound&&!matches!(io.raw_os_error(),Some(libc::ENOTDIR)|Some(libc::ELOOP))){return error;}
    let tagged:anyhow::Error=ObservedObjectFailure(error).into();
    // Test cancellation exactly after the physical observation is tagged,
    // before a caller can make a later remaining-time query.
    #[cfg(test)]PHYSICAL_FAILURE_HOOK.with(|slot|{let hook=slot.borrow_mut().take();if let Some(hook)=hook{hook();}});
    tagged
})}
fn observed_failure(error:&anyhow::Error)->bool{error.chain().any(|cause|cause.is::<ObservedObjectFailure>())}


#[derive(Clone,Debug,PartialEq,Eq)]
struct Identity {dev:u64,ino:u64,mode:u32,uid:u32,gid:u32}
fn identity(file:&File)->Result<Identity>{let m=file.metadata()?;Ok(Identity{dev:m.dev(),ino:m.ino(),mode:m.mode(),uid:m.uid(),gid:m.gid()})}
#[derive(Clone,PartialEq,Eq)]
struct Version {id:Identity,len:u64,links:u64,mtime:(i64,i64),ctime:(i64,i64)}
fn version(file:&File)->Result<Version>{let m=file.metadata()?;Ok(Version{id:identity(file)?,len:m.len(),links:m.nlink(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})}
fn at(parent:&File,name:&str,directory:bool)->std::io::Result<File>{
    if name.is_empty()||name=="."||name==".."||name.contains(['/', '\0']){return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,"invalid backed child"));}
    let name=CString::new(name).map_err(|e|std::io::Error::new(std::io::ErrorKind::InvalidInput,e))?;
    let fd=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK|if directory{libc::O_DIRECTORY}else{0})};
    if fd<0{return Err(std::io::Error::last_os_error())}Ok(unsafe{File::from_raw_fd(fd)})
}
fn directory(file:&File)->Result<Identity>{let id=identity(file)?;ensure!(id.mode&libc::S_IFMT==libc::S_IFDIR&&id.uid==unsafe{libc::geteuid()}&&id.mode&0o022==0,"backed local directory not current private owner");Ok(id)}
struct StoreIo {store:LocalStore,original:Arc<dyn ImageImportFence>,root:Option<File>,objects:Option<File>,tmp:Option<File>,gc:Option<File>,
    actual_local_hash_reads:u64,actual_local_hash_bytes:u64,actual_local_object_syncs:u64,actual_local_object_sync_bytes:u64,
    ids:Option<[Identity;4]>,locked:bool,shards:BTreeMap<String,(File,Identity)>,pending:Option<File>,pending_name:Option<String>,partial:Vec<u8>,
    directory_io:Option<File>,intent:Option<RetentionState>,durable_reads:bool,foreign:Option<VerifiedBakedRead>,required_object:bool,
    #[cfg(test)] object_read_bytes:BTreeMap<ObjectRef,u64>,#[cfg(test)] object_syncs:BTreeMap<ObjectRef,u64>,
    #[cfg(test)] fail_after_rename:bool,#[cfg(test)]capture_open_error:Option<i32>}
impl StoreIo {
    fn staged(store:LocalStore,original:Arc<dyn ImageImportFence>)->Self{Self{store,original,root:None,objects:None,tmp:None,gc:None,ids:None,locked:false,
        actual_local_hash_reads:0,actual_local_hash_bytes:0,actual_local_object_syncs:0,actual_local_object_sync_bytes:0,
        shards:BTreeMap::new(),pending:None,pending_name:None,partial:Vec::new(),directory_io:None,intent:None,durable_reads:false,foreign:None,required_object:false,#[cfg(test)]object_read_bytes:BTreeMap::new(),#[cfg(test)]object_syncs:BTreeMap::new(),#[cfg(test)]fail_after_rename:false,#[cfg(test)]capture_open_error:None}}
    fn capture(&mut self)->Result<()> {
        self.original.remaining()?;ensure!(self.root.is_none(),"backed store capture repeated");
        let named=observed(self.store.root.canonicalize().map_err(Into::into))?;
        observed((||->Result<()>{ensure!(named==self.store.root,"backed store capture redirected");Ok(())})())?;
        #[cfg(test)]if let Some(errno)=self.capture_open_error.take(){return observed(Err(std::io::Error::from_raw_os_error(errno).into()));}
        self.root=Some(observed(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&self.store.root).map_err(Into::into))?);
        let root=observed(directory(self.root.as_ref().unwrap()))?;self.objects=Some(observed(at(self.root.as_ref().unwrap(),"objects",true).map_err(Into::into))?);let objects=observed(directory(self.objects.as_ref().unwrap()))?;
        self.tmp=Some(observed(at(self.root.as_ref().unwrap(),"tmp",true).map_err(Into::into))?);let tmp=observed(directory(self.tmp.as_ref().unwrap()))?;self.gc=Some(observed(at(self.root.as_ref().unwrap(),"lock",false).map_err(Into::into))?);
        let m=self.gc.as_ref().unwrap().metadata()?;let gc=identity(self.gc.as_ref().unwrap())?;
        observed((||->Result<()>{ensure!(m.is_file()&&m.nlink()==1&&m.len()==0&&m.uid()==unsafe{libc::geteuid()}&&m.mode()&0o022==0,"backed actual GC file unsafe");Ok(())})())?;
        self.ids=Some([root,objects,tmp,gc]);self.current()
    }
    fn current(&self)->Result<()> {
        self.original.remaining()?;let ids=self.ids.as_ref().context("backed original store capture incomplete")?;
        observed((||->Result<()>{ensure!(self.store.root.canonicalize()?==self.store.root,"backed original store redirects");
        let named=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&self.store.root)?;
        ensure!(directory(&named)?==ids[0]&&directory(self.root.as_ref().unwrap())?==ids[0],"backed original store replaced");
        for(name,held,id)in [("objects",self.objects.as_ref().unwrap(),&ids[1]),("tmp",self.tmp.as_ref().unwrap(),&ids[2])]{
            ensure!(directory(held)?==*id&&directory(&at(self.root.as_ref().unwrap(),name,true)?)?==*id,"backed original store child changed");}
        ensure!(identity(self.gc.as_ref().unwrap())?==ids[3]&&identity(&at(self.root.as_ref().unwrap(),"lock",false)?)?==ids[3],"backed original GC inode changed");Ok(())})())?;
        self.original.remaining()?;Ok(())
    }
    fn acquire(&mut self,exclusive:bool)->Result<()> {self.capture()?;loop{self.current()?;
        let result=if exclusive{FileExt::try_lock_exclusive(self.gc.as_ref().unwrap())}else{FileExt::try_lock_shared(self.gc.as_ref().unwrap())};
        match result{Ok(())=>{self.locked=true;return self.current()},Err(e)if e.kind()==std::io::ErrorKind::WouldBlock=>std::thread::sleep(self.original.remaining()?.min(Duration::from_millis(1))),Err(e)=>return Err(e.into())}}
    }
    fn shard(&mut self,name:&str)->Result<bool>{self.current()?;if !self.shards.contains_key(name){
        let file=match at(self.objects.as_ref().unwrap(),name,true){Ok(f)=>f,Err(e)if e.kind()==std::io::ErrorKind::NotFound=>{if self.required_object{return observed(Err(anyhow::anyhow!("required original local CAS shard absent")))}self.current()?;return Ok(false)},Err(e)=>return observed(Err(e.into()))};
        let id=observed(directory(&file))?;ensure!(self.shards.len()<256,"backed shard FD bound");self.shards.insert(name.into(),(file,id));}
        let(file,id)=&self.shards[name];observed((||->Result<()>{ensure!(directory(file)?==*id&&directory(&at(self.objects.as_ref().unwrap(),name,true)?)?==*id,"backed original shard changed");Ok(())})())?;Ok(true)}
    fn read_pending(&mut self,maximum:u64,immutable:bool)->Result<()> {
        self.current()?;let file=self.pending.as_ref().context("backed actual pending file absent")?;let before=observed((||->Result<Version>{let before=version(file)?;
        ensure!(before.id.mode&libc::S_IFMT==libc::S_IFREG&&before.id.uid==unsafe{libc::geteuid()}&&before.links==1&&before.len<=maximum
            &&(if immutable{before.id.mode&0o7777==0o444}else{before.id.mode&0o077==0}),"backed local file type/mode/size changed");Ok(before)})())?;
        self.partial.clear();self.partial.try_reserve_exact(usize::try_from(before.len)?)?;let mut chunk=vec![0;64*1024];
        loop{self.original.remaining()?;let n=observed(self.pending.as_mut().unwrap().read(&mut chunk).map_err(Into::into))?;if n==0{break;}
            observed((||->Result<()>{ensure!((self.partial.len()as u64).checked_add(n as u64).is_some_and(|n|n<=before.len),"backed object grew");Ok(())})())?;self.partial.extend_from_slice(&chunk[..n]);}
        observed((||->Result<()>{ensure!(self.partial.len()as u64==before.len&&version(self.pending.as_ref().unwrap())?==before,"backed object changed during read");Ok(())})())?;self.current()?;
        Ok(())
    }
    fn read_state(&mut self)->Result<RetentionState>{ensure!(self.locked&&self.pending.is_none(),"backed state requires actual original GC owner");
        self.pending=Some(observed(at(self.root.as_ref().unwrap(),"retention.json",false).map_err(Into::into))?);self.read_pending(RETENTION,false)?;
        let envelope=observed((||->Result<StateEnvelope>{
            let envelope:StateEnvelope=serde_json::from_slice(&self.partial)?;ensure!(canonical_json(&envelope)?==self.partial,"backed retention envelope not canonical");
            ensure!(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&envelope.state)?).digest==envelope.checksum,"backed retention checksum differs");
            ensure!(matches!(envelope.state.format_version,1|2),"unsupported base retention version");if envelope.state.format_version==2{object_sets::validate_presence(&self.store)?;}
            for(name,r)in &envelope.state.pins{validate_pin(name)?;r.validate()?;}for(id,lease)in &envelope.state.staging{ensure!(*id==lease.id&&!id.is_empty(),"backed staging DATA invalid");}
            image_import::validate_holds(&envelope.state.image_import_holds)?;backed_data::validate_retention(&envelope.state)?;Ok(envelope)
        })())?;self.current()?;
        observed((||->Result<()>{ensure!(version(&at(self.root.as_ref().unwrap(),"retention.json",false)?)?==version(self.pending.as_ref().unwrap())?,"backed retention name changed during read");Ok(())})())?;
        self.pending.take();self.partial.clear();Ok(envelope.state)
    }
    fn read_object(&mut self,r:&ObjectRef,maximum:u64)->Result<Option<Vec<u8>>>{self.current()?;ensure!(self.locked&&self.pending.is_none(),"backed object read GC/pending owner absent");r.validate()?;
        let shard=&r.digest[..2];if !self.shard(shard)?{return Ok(None)}
        self.pending=match at(&self.shards[shard].0,&r.digest[2..],false){Ok(f)=>Some(f),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>{if self.required_object{return observed(Err(anyhow::anyhow!("required original local CAS object absent")))}self.current()?;return Ok(None)},Err(e)=>return observed(Err(e.into()))};
        observed((||->Result<()>{ensure!(self.pending.as_ref().unwrap().metadata()?.len()==r.size_bytes,"backed local object size differs");Ok(())})())?;self.read_pending(maximum,true)?;
        #[cfg(test)]{*self.object_read_bytes.entry(r.clone()).or_default()+=self.partial.len()as u64;}
        observed(r.verify(&self.partial))?;
        self.actual_local_hash_reads=self.actual_local_hash_reads.checked_add(1).context("actual local hash count overflow")?;
        self.actual_local_hash_bytes=self.actual_local_hash_bytes.checked_add(self.partial.len()as u64).context("actual local hash bytes overflow")?;
        observed(metadata_references(r.kind,&self.partial))?;self.current()?;
        if self.durable_reads{sync_file(self.pending.as_ref().unwrap())?;
            self.actual_local_object_syncs=self.actual_local_object_syncs.checked_add(1).context("actual local object sync count overflow")?;
            self.actual_local_object_sync_bytes=self.actual_local_object_sync_bytes.checked_add(self.partial.len()as u64).context("actual local object sync bytes overflow")?;
            #[cfg(test)]{*self.object_syncs.entry(r.clone()).or_default()+=1;}
            self.current()?;}
        observed((||->Result<()>{ensure!(version(&at(&self.shards[shard].0,&r.digest[2..],false)?)?==version(self.pending.as_ref().unwrap())?,"backed local object name changed");Ok(())})())?;
        self.pending.take();Ok(Some(std::mem::take(&mut self.partial)))}
    // A required local obligation cannot be substituted from backing. Tag a
    // missing name before any later clock callback; leave the original pending
    // descriptor and required-read disposition in this carrier on failure.
    fn read_required_object(&mut self,r:&ObjectRef,maximum:u64)->Result<Vec<u8>> {
        ensure!(!self.required_object,"original required local read still unsettled");
        self.required_object=true;
        let raw=self.read_object(r,maximum)?.context("required original local object absent")?;
        self.required_object=false;Ok(raw)
    }
    fn sync_directory(&mut self,file:File)->Result<()> {self.directory_io=Some(file);self.current()?;sync_file(self.directory_io.as_ref().unwrap())?;self.current()?;self.directory_io.take();Ok(())}
    fn write_state(&mut self,mut state:RetentionState)->Result<RetentionState>{self.current()?;ensure!(self.locked&&self.pending.is_none(),"backed state writer not original GC owner");
        state.generation=state.generation.checked_add(1).context("backed retention generation overflow")?;backed_data::validate_retention(&state)?;
        let bytes=canonical_json(&StateEnvelope{checksum:ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&state)?).digest,state:state.clone()})?;
        ensure!(bytes.len()as u64<=RETENTION,"backed retention encoding exceeds bound");self.intent=Some(state.clone());let name=format!("backed-{}",Uuid::new_v4());self.pending_name=Some(name.clone());
        let c=CString::new(name)?;let fd=unsafe{libc::openat(self.tmp.as_ref().unwrap().as_raw_fd(),c.as_ptr(),libc::O_WRONLY|libc::O_CREAT|libc::O_EXCL|libc::O_NOFOLLOW|libc::O_CLOEXEC,0o600)};
        ensure!(fd>=0,"backed original state temporary open failed");self.pending=Some(unsafe{File::from_raw_fd(fd)});
        for chunk in bytes.chunks(64*1024){self.current()?;self.pending.as_mut().unwrap().write_all(chunk)?;}
        self.current()?;sync_file(self.pending.as_ref().unwrap())?;self.current()?;
        ensure!(unsafe{libc::renameat(self.tmp.as_ref().unwrap().as_raw_fd(),c.as_ptr(),self.root.as_ref().unwrap().as_raw_fd(),c"retention.json".as_ptr())}==0,"backed retention rename outcome failed");
        #[cfg(test)] if self.fail_after_rename{bail!("injected backed state directory sync unknown")}
        self.sync_directory(self.root.as_ref().unwrap().try_clone()?)?;self.sync_directory(self.tmp.as_ref().unwrap().try_clone()?)?;
        self.current()?;self.pending.take();self.pending_name=None;Ok(state)
    }
    fn release_known(&mut self)->Result<()> {self.original.remaining()?;
        ensure!(self.pending.is_none()&&self.pending_name.is_none()&&self.directory_io.is_none()&&self.foreign.is_none(),
            "known backed IO settlement still has original pending resources");
        if self.locked{FileExt::unlock(self.gc.as_ref().context("original GC descriptor absent at settlement")?)?;self.locked=false;}
        self.original.remaining()?;self.gc.take();
        // Synchronous reads, durability barriers and original postchecks are
        // complete. These per-operation cache handles are not pin custody:
        // StoreCustody retains the actual root/objects/tmp/GC descriptors.
        // Failed/late paths above keep every shard and pending IO unchanged.
        self.shards.clear();Ok(())}
}
/// Held original local descriptors, not reconstructed inode DATA. The GC FD
/// is duplicated before known unlock and retained afterwards WITHOUT a lock.
struct StoreCustody {files:[File;4],ids:[Identity;4]}
impl StoreCustody {
    fn capture(io:&StoreIo)->Result<Arc<Self>> {io.current()?;let value=Arc::new(Self{files:[io.root.as_ref().unwrap().try_clone()?,io.objects.as_ref().unwrap().try_clone()?,io.tmp.as_ref().unwrap().try_clone()?,io.gc.as_ref().unwrap().try_clone()?],ids:io.ids.as_ref().unwrap().clone()});value.current(io)?;Ok(value)}
    fn current(&self,io:&StoreIo)->Result<()> {io.current()?;
        observed((||->Result<()>{ensure!(io.ids.as_ref()==Some(&self.ids),"actual retained local store descriptors differ");
            for(index,file)in self.files.iter().enumerate(){ensure!(identity(file)?==self.ids[index],"actual retained original local descriptor changed");}Ok(())})())?;
        io.original.remaining()?;Ok(())}
}
fn bound(r:&ObjectRef,sources:&[Arc<VerifiedBakedInputs>])->Result<u64>{r.validate()?;let maximum=match r.kind{
    ObjectKind::Blob=>if sources.iter().any(|s|s.binding_data().is_ok_and(|b|b.manifest==*r)){32*1024*1024}else{4*1024*1024},
    ObjectKind::Directory|ObjectKind::DirectoryShard|ObjectKind::File=>64*1024,_=>1024*1024};
    ensure!(r.size_bytes<=maximum&&(r.kind==ObjectKind::Blob||r.size_bytes>0),"backed object exceeds typed profile");Ok(maximum)}
fn sources_current(sources:&[Arc<VerifiedBakedInputs>],original:&dyn ImageImportFence)->Result<()> {original.remaining()?;for s in sources{s.current(original)?;}original.remaining()?;Ok(())}
struct Graph {all:BTreeSet<ObjectRef>,local:BTreeSet<ObjectRef>,used:BTreeSet<String>}
// Publication classifies local-first. Explicit reattachment/GC instead replay
// the original partition: a newly present local replica never replaces the
// recorded foreign obligation. Every present local copy is still verified.
fn graph(io:&mut StoreIo,roots:Vec<ObjectRef>,sources:&[Arc<VerifiedBakedInputs>],partition:Option<&BackedPinData>)->Result<Graph>{
    let recorded_local:Option<BTreeSet<_>>=partition.map(|p|p.local_objects.iter().cloned().collect());
    sources_current(sources,&*io.original)?;let mut graph=Graph{all:BTreeSet::new(),local:BTreeSet::new(),used:BTreeSet::new()};let mut pending=roots;let mut metadata=BTreeMap::new();let mut work=0u64;let mut total=0u64;
    while let Some(r)=pending.pop(){io.current()?;if !graph.all.insert(r.clone()){continue;}ensure!(graph.all.len()<=REFS,"backed complete graph count bound");
        let max=bound(&r,sources)?;total=total.checked_add(r.size_bytes).context("backed graph bytes overflow")?;ensure!(total<=2*1024*1024*1024,"backed complete graph bytes bound");
        let local=io.read_object(&r,max)?;
        let requires_local=recorded_local.as_ref().map_or(local.is_some(),|set|set.contains(&r));
        let raw=if requires_local{graph.local.insert(r.clone());Some(local.context("recorded local graph child absent; backing cannot replace its obligation")?)}else{
            let source=sources.iter().filter(|s|s.contains_data(&r)).min_by_key(|s|s.binding_data().unwrap().id_data().unwrap()).context("recorded foreign graph child absent from original actual backing")?;
            source.current(&*io.original)?;graph.used.insert(source.binding_data()?.id_data()?);
            if partition.is_some(){
                // Fresh full revalidation reads original foreign bytes even
                // when a valid local copy exists. Failed reads remain owned.
                ensure!(io.foreign.is_none(),"original backed foreign validation still pending");
                source.read_retaining(&mut io.foreign,r.clone(),io.original.clone())?;
                let raw=if r.kind==ObjectKind::Blob{None}else{Some(io.foreign.as_ref().unwrap().verified_bytes()?.to_vec())};
                io.original.remaining()?;io.foreign.take();raw
            }else if r.kind==ObjectKind::Blob{None}else{Some(source.metadata(&r)?.to_vec())}};
        if r.kind!=ObjectKind::Blob {let raw=raw.context("actual metadata unavailable")?;work=work.checked_add(raw.len()as u64).context("backed metadata work overflow")?;
            ensure!(work<=WORK,"backed metadata work bound");pending.extend(metadata_references(r.kind,&raw)?);ensure!(pending.len()<=REFS*4,"backed graph frontier bound");metadata.insert(r,raw);}
    }
    let reader=Metadata{bytes:&metadata,original:&*io.original};validate_edges(&reader,&metadata,&graph.all)?;sources_current(sources,&*io.original)?;Ok(graph)
}
fn graph_digest(graph:&Graph)->Result<String>{Ok(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&graph.all)?).digest)}
fn source_bindings(sources:&[Arc<VerifiedBakedInputs>],original:&dyn ImageImportFence)->Result<BTreeMap<String,BakedInputsBindingData>>{
    ensure!(!sources.is_empty()&&sources.len()<=backed_data::MAX_BACKINGS,"backed input owner count");sources_current(sources,original)?;let mut result=BTreeMap::new();
    for source in sources{let b=source.binding_data()?.clone();ensure!(result.insert(b.id_data()?,b).is_none(),"duplicate actual baked backing");}Ok(result)}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum BackedPublicationStatus {Captured,Holding,Validating,Publishing,Acknowledged,Failed}
/// Only original known synchronous publication can create this ACK. Keep the
/// whole publication on Err/panic; no retry or readback-ACK path exists.
pub struct BackedPublication {io:StoreIo,sources:Vec<Arc<VerifiedBakedInputs>>,staging_id:String,pin:String,root:ObjectRef,operation:String,
    status:BackedPublicationStatus,record:Option<BackedPinData>,ack:Option<Arc<BackedPinOwner>>,failure:Option<anyhow::Error>,
    version_before:Vec<Option<super::baked_image::BakedVersionSweep>>,version_after:Vec<Option<super::baked_image::BakedVersionSweep>>,
    #[cfg(test)]fault:Option<TestPoint>}
/// A live actual storage DATA owner. It holds original backing/graph/local
/// identity. It is neither a serialized pin nor Root execution permission.
pub struct BackedPinOwner {store:LocalStore,custody:Arc<StoreCustody>,pin:String,record:BackedPinData,sources:Vec<Arc<VerifiedBakedInputs>>,members:BTreeSet<ObjectRef>,origin:BackedOwnerOrigin}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum BackedOwnerOrigin {OriginalPublication,ExplicitReattachment}
impl BackedPinOwner {
    pub fn root_data(&self)->&ObjectRef{&self.record.root}
    pub fn record_data(&self)->&BackedPinData{&self.record}
    pub fn origin_data(&self)->BackedOwnerOrigin{self.origin}
    fn check(&self,io:&StoreIo,state:&RetentionState)->Result<()> {io.current()?;ensure!(io.store.root==self.store.root,"backed owner belongs to another local store");self.custody.current(io)?;
        let data=state.backed_retention.as_ref().context("backed actual retention absent")?;ensure!(data.pins.get(&self.pin)==Some(&self.record),"backed current record absent/replaced");
        for source in &self.sources {source.current(&*io.original)?;let b=source.binding_data()?;ensure!(data.bindings.get(&b.id_data()?)==Some(b),"backed original source binding changed");}io.current()
    }
}
impl LocalStore {
    pub fn publish_backed_retaining(&self,out:&mut Option<BackedPublication>,staging_id:String,pin:String,root:ObjectRef,sources:Vec<Arc<VerifiedBakedInputs>>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"backed original publication output occupied");*out=Some(BackedPublication{io:StoreIo::staged(self.clone(),original),sources,staging_id,pin,root,operation:Uuid::new_v4().to_string(),status:BackedPublicationStatus::Captured,record:None,ack:None,failure:None,version_before:Vec::new(),version_after:Vec::new(),#[cfg(test)]fault:None});out.as_mut().unwrap().run()
    }
}
impl BackedPublication {
    pub fn status_data(&self)->BackedPublicationStatus{self.status}
    pub fn acknowledgment(&self)->Result<&Arc<BackedPinOwner>>{ensure!(self.status==BackedPublicationStatus::Acknowledged,"original backed publication ACK absent/unknown");self.ack.as_ref().context("backed original ACK absent")}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    fn run(&mut self)->Result<()> {ensure!(self.status==BackedPublicationStatus::Captured,"original backed publication cannot retry");let result=self.run_inner();
        if let Err(e)=result{self.status=BackedPublicationStatus::Failed;self.failure=Some(e);bail!("original backed publication failed; retain carrier")}Ok(())}
    fn run_inner(&mut self)->Result<()> {
        validate_pin(&self.pin)?;let bindings=source_bindings(&self.sources,&*self.io.original)?;
        // Typed closure + unchanged original immutable versions, not a claim
        // that all foreign payloads were rehashed. Reads still verify SHA.
        for source in &self.sources{self.version_before.push(None);source.check_original_versions_retaining(self.version_before.last_mut().unwrap(),self.io.original.clone())?;}
        self.io.acquire(true)?;let mut state=self.io.read_state()?;
        ensure!(state.staging.get(&self.staging_id).is_some_and(|l|l.expires_at_ms>now_ms().unwrap_or(u64::MAX)),"backed publication original live staging absent");
        ensure!(!state.pins.contains_key(&self.pin),"backed name collides with ordinary complete-local pin");let data=state.backed_retention.get_or_insert_with(BackedRetentionData::default);
        ensure!(!data.pins.contains_key(&self.pin)&&!data.holds.values().any(|h|h.pin==self.pin)&&data.holds.len()<backed_data::MAX_BACKED_HOLDS,"backed pin/intent already exists; original recovery required");
        for(id,b)in &bindings{if let Some(old)=data.bindings.get(id){ensure!(old==b,"backed binding collision");}else{data.bindings.insert(id.clone(),b.clone());}}
        data.holds.insert(self.operation.clone(),BackedHoldData{format_version:1,operation_id:self.operation.clone(),pin:self.pin.clone(),root:self.root.clone(),staging_id:self.staging_id.clone(),backing_ids:bindings.keys().cloned().collect()});
        self.status=BackedPublicationStatus::Holding;state=self.io.write_state(state)?;
        #[cfg(test)]self.point(TestPoint::AfterHold)?;
        self.status=BackedPublicationStatus::Validating;self.io.durable_reads=true;let graph=graph(&mut self.io,vec![self.root.clone()],&self.sources,None)?;
        ensure!(!graph.used.is_empty(),"backed publication contains no actual foreign input; use ordinary local publication");
        let shards:Vec<_>=self.io.shards.values().map(|(f,_)|f.try_clone()).collect::<std::io::Result<_>>()?;
        for shard in shards{self.io.sync_directory(shard)?;}self.io.sync_directory(self.io.objects.as_ref().unwrap().try_clone()?)?;self.io.sync_directory(self.io.tmp.as_ref().unwrap().try_clone()?)?;
        for source in &self.sources{self.version_after.push(None);source.check_original_versions_retaining(self.version_after.last_mut().unwrap(),self.io.original.clone())?;}
        ensure!(state.staging[&self.staging_id].expires_at_ms>now_ms()?,"backed original staging expired");
        let record=BackedPinData{format_version:1,operation_id:self.operation.clone(),root:self.root.clone(),local_objects:graph.local.iter().cloned().collect(),backing_ids:graph.used.iter().cloned().collect(),graph_sha256:graph_digest(&graph)?,generation:state.generation.checked_add(1).context("backed generation overflow")?};
        self.record=Some(record.clone());let data=state.backed_retention.as_mut().unwrap();data.pins.insert(self.pin.clone(),record.clone());data.holds.remove(&self.operation);
        let needed:BTreeSet<_>=data.pins.values().flat_map(|p|p.backing_ids.iter()).chain(data.holds.values().flat_map(|p|p.backing_ids.iter())).cloned().collect();data.bindings.retain(|id,_|needed.contains(id));state.staging.remove(&self.staging_id);
        self.status=BackedPublicationStatus::Publishing;
        #[cfg(test)]if self.fault==Some(TestPoint::FinalRename){self.io.fail_after_rename=true;}
        state=self.io.write_state(state)?;
        #[cfg(test)]self.point(TestPoint::AfterCommit)?;
        let owner=Arc::new(BackedPinOwner{store:self.io.store.clone(),custody:StoreCustody::capture(&self.io)?,pin:self.pin.clone(),record,sources:self.sources.iter().filter(|s|graph.used.contains(&s.binding_data().unwrap().id_data().unwrap())).cloned().collect(),members:graph.all,origin:BackedOwnerOrigin::OriginalPublication});
        self.ack=Some(owner);self.ack.as_ref().unwrap().check(&self.io,&state)?;self.io.original.remaining()?;
        self.io.release_known()?;self.status=BackedPublicationStatus::Acknowledged;Ok(())
    }
    #[cfg(test)]fn point(&self,p:TestPoint)->Result<()>{ensure!(self.fault!=Some(p),"injected original backed outcome loss");Ok(())}
}

/// A fresh, explicit full revalidation. This is NOT the old publication ACK;
/// no original pin/SQL outcome is reconstructed, resent or made timely.
pub struct BackedReattachment {io:StoreIo,sources:Vec<Arc<VerifiedBakedInputs>>,pin:String,owner:Option<Arc<BackedPinOwner>>,attempted:bool,ready:bool,failure:Option<anyhow::Error>}
impl LocalStore {
    pub fn reattach_backed_retaining(&self,out:&mut Option<BackedReattachment>,pin:String,sources:Vec<Arc<VerifiedBakedInputs>>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"backed reattachment output occupied");*out=Some(BackedReattachment{io:StoreIo::staged(self.clone(),original),sources,pin,owner:None,attempted:false,ready:false,failure:None});out.as_mut().unwrap().run()
    }
}
impl BackedReattachment {
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    pub fn reattached_owner(&self)->Result<&Arc<BackedPinOwner>>{ensure!(self.ready,"explicit backed reattachment incomplete");self.owner.as_ref().context("reattached actual owner absent")}
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"original reattachment cannot retry");self.attempted=true;let result=self.run_inner();if let Err(e)=result{self.failure=Some(e);bail!("backed reattachment failed; retain actual carrier")}Ok(())}
    fn run_inner(&mut self)->Result<()> {
        validate_pin(&self.pin)?;let supplied=source_bindings(&self.sources,&*self.io.original)?;self.io.acquire(true)?;let state=self.io.read_state()?;
        let data=state.backed_retention.as_ref().context("record alone cannot invent backed retention")?;
        ensure!(data.holds.is_empty(),"partial/unknown original backed mutation still held; no reattachment promotion");
        let record=data.pins.get(&self.pin).context("backed reattachment pin absent")?.clone();
        ensure!(record.backing_ids.iter().cloned().collect::<BTreeSet<_>>()==supplied.keys().cloned().collect()
            &&supplied.iter().all(|(id,b)|data.bindings.get(id)==Some(b)),"actual current backing differs from recorded DATA");
        self.io.durable_reads=true;let graph=graph(&mut self.io,vec![record.root.clone()],&self.sources,Some(&record))?;
        ensure!(graph.local.iter().cloned().collect::<Vec<_>>()==record.local_objects&&graph.used.iter().cloned().collect::<Vec<_>>()==record.backing_ids&&graph_digest(&graph)?==record.graph_sha256,
            "reattached local/foreign partition differs from original complete record");
        let shards=self.io.shards.values().map(|(f,_)|f.try_clone()).collect::<std::io::Result<Vec<_>>>()?;
        for shard in shards{self.io.sync_directory(shard)?;}self.io.sync_directory(self.io.objects.as_ref().unwrap().try_clone()?)?;self.io.sync_directory(self.io.tmp.as_ref().unwrap().try_clone()?)?;
        self.owner=Some(Arc::new(BackedPinOwner{store:self.io.store.clone(),custody:StoreCustody::capture(&self.io)?,pin:self.pin.clone(),record,sources:self.sources.clone(),members:graph.all,origin:BackedOwnerOrigin::ExplicitReattachment}));
        self.owner.as_ref().unwrap().check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())
    }
}

/// Bounded original object read; the owner and failed local/foreign IO remain
/// in the output before checks. No raw backing FD/path fallback is exposed.
pub struct BackedRead {owner:Arc<BackedPinOwner>,io:StoreIo,reference:ObjectRef,local:Option<Vec<u8>>,foreign:Option<VerifiedBakedRead>,attempted:bool,ready:bool}
impl BackedPinOwner {
    pub fn read_retaining(self:&Arc<Self>,out:&mut Option<BackedRead>,reference:ObjectRef,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"backed original read occupied");*out=Some(BackedRead{owner:self.clone(),io:StoreIo::staged(self.store.clone(),original),reference,local:None,foreign:None,attempted:false,ready:false});out.as_mut().unwrap().run()
    }
}
impl BackedRead {
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"backed original read cannot retry");self.attempted=true;self.io.acquire(false)?;let state=self.io.read_state()?;self.owner.check(&self.io,&state)?;
        ensure!(self.owner.members.contains(&self.reference),"read outside genuine backed root graph");let maximum=bound(&self.reference,&self.owner.sources)?;
        if let Some(bytes)=self.io.read_object(&self.reference,maximum)?{self.local=Some(bytes);}else{
            let source=self.owner.sources.iter().find(|s|s.contains_data(&self.reference)).context("actual backed read missing from local and original sources")?;
            source.read_retaining(&mut self.foreign,self.reference.clone(),self.io.original.clone())?;}
        self.owner.check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())}
    pub fn verified_bytes(&self)->Result<&[u8]>{ensure!(self.ready,"backed read incomplete/unknown");match(&self.local,&self.foreign){(Some(bytes),None)=>Ok(bytes),(None,Some(read))=>read.verified_bytes(),_=>bail!("backed actual read disposition absent")}}
}

struct Candidate {shard:String,leaf:String,version:Version,size:u64}
/// Holds every actual backing, GC lock, SQLite read snapshot, partial read and
/// unlink intent through failure. No Drop IO or implicit capacity release.
pub struct BackedGc {io:StoreIo,owners:Vec<Arc<BackedPinOwner>>,dependency_owners:Vec<Arc<environment_dependencies::EnvironmentDependentPinOwner>>,source_owners:Vec<Arc<environment_dependencies::SourceOverlayPinOwner>>,grace:Duration,report:Option<GcReport>,object_sets:Option<RetainedObjectSetScan>,
    candidates:Vec<Candidate>,unlink_intent:Option<usize>,attempted:bool,ready:bool,failure:Option<anyhow::Error>}
impl LocalStore {
    pub fn gc_backed_retaining(&self,out:&mut Option<BackedGc>,owners:Vec<Arc<BackedPinOwner>>,grace:Duration,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"backed GC original output occupied");*out=Some(BackedGc{io:StoreIo::staged(self.clone(),original),owners,dependency_owners:Vec::new(),source_owners:Vec::new(),grace,report:None,object_sets:None,candidates:Vec::new(),unlink_intent:None,attempted:false,ready:false,failure:None});out.as_mut().unwrap().run()
    }
}
impl BackedGc {
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    pub fn acknowledgment(&self)->Result<&GcReport>{ensure!(self.ready,"backed GC original completion ACK absent");self.report.as_ref().context("backed GC report absent")}
    pub fn partial_report_data(&self)->Option<&GcReport>{self.report.as_ref()}
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"original backed GC cannot retry");self.attempted=true;let result=self.run_inner();if let Err(e)=result{self.failure=Some(e);bail!("original backed GC failed; retain partial effects and owners")}Ok(())}
    fn owners_current(&self,state:&RetentionState)->Result<()> {
        self.io.current()?;let pins=state.backed_retention.as_ref().map(|d|d.pins.len()).unwrap_or(0);
        ensure!(pins==self.owners.len()&&self.owners.len()<=backed_data::MAX_BACKED_PINS,"actual current backing owner absent/extra; no GC");
        let mut names=BTreeSet::new();for owner in &self.owners{ensure!(names.insert(owner.pin.clone()),"duplicate actual backed GC owner");owner.check(&self.io,state)?;}
        if let Some(data)=&state.backed_retention{ensure!(data.holds.is_empty(),"unresolved backed original hold blocks all GC");}
        environment_dependencies::gc_check(&self.dependency_owners,&self.io,state)?;
        environment_dependencies::source::gc_check(&self.source_owners,&self.io,state)?;
        self.io.current()
    }
    fn run_inner(&mut self)->Result<()> {
        let initial=(||->Result<RetentionState>{self.io.acquire(true)?;self.io.read_state()})();
        if initial.as_ref().err().is_some_and(observed_failure){environment_dependencies::source::fence_initial_io(&self.dependency_owners,&self.source_owners,self.io.store.root());}
        let state=initial?;self.report=Some(GcReport{generation:state.generation,retained_objects:0,removed_objects:0,removed_bytes:0,blocked_by_staging:false});self.owners_current(&state)?;
        if !state.image_import_holds.is_empty()||state.staging.values().any(|l|l.expires_at_ms>now_ms().unwrap_or(0)){
            self.report.as_mut().unwrap().blocked_by_staging=true;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;return Ok(())}
        let mut retained=BTreeSet::new();
        for root in state.pins.values(){let normal=graph(&mut self.io,vec![root.clone()],&[],None)?;retained.extend(normal.local.into_iter().map(|r|r.digest));}
        for owner in &self.owners{owner.check(&self.io,&state)?;
            let current=graph(&mut self.io,vec![owner.record.root.clone()],&owner.sources,Some(&owner.record))?;
            ensure!(current.local.iter().cloned().collect::<Vec<_>>()==owner.record.local_objects
                &&current.used.iter().cloned().collect::<Vec<_>>()==owner.record.backing_ids
                &&graph_digest(&current)?==owner.record.graph_sha256,"backed GC original partition differs");
            retained.extend(current.local.into_iter().map(|r|r.digest));}
        retained.extend(environment_dependencies::gc_local(&self.dependency_owners,&mut self.io,&state)?);
        retained.extend(environment_dependencies::source::gc_local(&self.source_owners,&mut self.io,&state)?);
        if state.format_version==2 {
            RetainedObjectSetScan::capture_retaining(&mut self.object_sets,self.io.store.clone(),self.io.original.clone())?;
            for r in self.object_sets.as_ref().unwrap().references()?.to_vec(){ensure!(self.io.read_object(&r,bound(&r,&[])?)?.is_some(),"explicit retained object disappeared");retained.insert(r.digest);}
        }
        self.report.as_mut().unwrap().retained_objects=retained.len();
        // Validate EVERY candidate before the first unlink. No parse, missing
        // owner, raw hash, schema or clock failure can produce a partial sweep.
        let mut count=0usize;
        for shard in fs::read_dir(self.io.store.root.join("objects"))?{self.io.current()?;let shard=shard?;ensure!(shard.file_type()?.is_dir(),"backed GC shard is not directory");
            let name=shard.file_name().into_string().map_err(|_|anyhow::anyhow!("backed GC shard not UTF8"))?;ensure!(name.len()==2&&is_lower_hex(&name)&&self.io.shard(&name)?,"backed GC invalid shard");
            for item in fs::read_dir(shard.path())?{self.io.current()?;let item=item?;ensure!(item.file_type()?.is_file(),"backed GC object type invalid");count+=1;ensure!(count<=131_072,"backed GC file count bound");
                let leaf=item.file_name().into_string().map_err(|_|anyhow::anyhow!("backed GC leaf not UTF8"))?;let digest=format!("{name}{leaf}");ensure!(digest.len()==64&&is_lower_hex(&digest),"backed GC object name invalid");
                ensure!(self.io.pending.is_none(),"backed original GC read still retained");self.io.pending=Some(at(&self.io.shards[&name].0,&leaf,false)?);
                let before=version(self.io.pending.as_ref().unwrap())?;let maximum=if self.owners.iter().flat_map(|o|o.sources.iter()).any(|s|s.binding_data().is_ok_and(|b|b.manifest.digest==digest)){32*1024*1024}else{4*1024*1024};
                self.io.read_pending(maximum,true)?;ensure!(ObjectRef::from_bytes(ObjectKind::Blob,&self.io.partial).digest==digest,"corrupt raw CAS object blocks backed GC");
                ensure!(version(&at(&self.io.shards[&name].0,&leaf,false)?)?==before,"backed GC object name changed");
                let old_enough=self.io.pending.as_ref().unwrap().metadata()?.modified()?.elapsed().is_ok_and(|age|age>=self.grace);
                if !retained.contains(&digest)&&old_enough{self.candidates.push(Candidate{shard:name.clone(),leaf,version:before,size:self.io.partial.len()as u64});}
                self.io.current()?;self.io.pending.take();self.io.partial.clear();
            }
        }
        self.owners_current(&state)?;
        for index in 0..self.candidates.len(){self.owners_current(&state)?;self.unlink_intent=Some(index);let candidate=&self.candidates[index];
            ensure!(self.io.pending.is_none()&&self.io.shard(&candidate.shard)?,"backed unlink parent missing");self.io.pending=Some(at(&self.io.shards[&candidate.shard].0,&candidate.leaf,false)?);
            ensure!(version(self.io.pending.as_ref().unwrap())?==candidate.version,"backed unlink original object changed");self.io.current()?;
            let leaf=CString::new(candidate.leaf.clone())?;ensure!(unsafe{libc::unlinkat(self.io.shards[&candidate.shard].0.as_raw_fd(),leaf.as_ptr(),0)}==0,"backed original unlink outcome failed");
            self.report.as_mut().unwrap().removed_objects+=1;self.report.as_mut().unwrap().removed_bytes+=candidate.size;
            self.io.sync_directory(self.io.shards[&candidate.shard].0.try_clone()?)?;self.io.pending.take();self.unlink_intent=None;
        }
        self.owners_current(&state)?;self.io.original.remaining()?;self.object_sets.take();self.io.release_known()?;self.ready=true;Ok(())
    }
}
#[cfg(test)]#[derive(Clone,Copy,PartialEq,Eq)]enum TestPoint{AfterHold,FinalRename,AfterCommit}
#[cfg(test)]#[path="backed_retention_tests.rs"]mod tests;

#[path="environment_dependencies.rs"]
pub mod environment_dependencies;
