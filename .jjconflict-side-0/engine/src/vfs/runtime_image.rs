//! Pure readonly runtime image DATA over the existing VFS. No Root/exec/pin
//! permission is issued here; the actual Root mount issuer is independent.
use super::*;
use crate::{content::{decode_file_node,validate_file_child},directory::{self as paged,DirectoryStore,PagedDirectory},
    execution::finalization_budget::FinalizationBudget,
    store::{ImageImportFence,baked_image::BakedImageCas,composed::{ComposedImageStore,ComposedRead,ComposedReadSession,ComposedSessionState}}};
use std::sync::atomic::AtomicUsize;
const REQUESTS:usize=8;
const OBJECTS:usize=65_536;
const METADATA:u64=128*1024*1024;
/// Restriction only. Implemented by the actual Root issuer's retained
/// admission owner. A callback/this DATA floor never grants mount or exec.
pub trait RuntimeImageReadRestriction:Send+Sync {
    fn check_new_read(&self,original:&dyn ImageImportFence)->Result<()>;
}
pub(super) struct Floor {nonce:String,root:ObjectRef,composed:Arc<ComposedImageStore>,_baked:Arc<BakedImageCas>,
    members:BTreeSet<ObjectRef>,groups:BTreeMap<String,(String,u64)>,restriction:Arc<dyn RuntimeImageReadRestriction>,
    closed:AtomicBool,requests:Mutex<Vec<Option<Arc<ReadRequest>>>>}
struct Clock {budget:FinalizationBudget,closed:AtomicBool}
impl ImageImportFence for Clock {fn remaining(&self)->Result<Duration>{
    ensure!(!self.closed.load(Ordering::Acquire),"runtime image request closed");self.budget.remaining()}}
pub(super) struct ReadRequest {nonce:String,clock:Arc<Clock>,session:ComposedReadSession,active:AtomicUsize,ready:AtomicBool}
impl ReadRequest {fn current(&self)->Result<()>{ensure!(self.ready.load(Ordering::Acquire),"runtime image request not admitted");self.clock.remaining()?;Ok(())}}
struct Active(Arc<ReadRequest>);
impl Drop for Active {fn drop(&mut self){self.0.active.fetch_sub(1,Ordering::AcqRel);}}
/// Root must retain this whole constructor carrier on every error/panic.
pub struct ReadonlyRuntimeImage {local:LocalStore,baked:Arc<BakedImageCas>,restriction:Arc<dyn RuntimeImageReadRestriction>,
    original:Arc<dyn ImageImportFence>,composed:Option<Arc<ComposedImageStore>>,pending:Option<ComposedRead>,
    metadata:BTreeMap<ObjectRef,Vec<u8>>,members:BTreeSet<ObjectRef>,index:Option<Connection>,db:Option<Connection>,
    floor:Option<Arc<Floor>>,view:Option<VfsView>,ready:bool}
impl ReadonlyRuntimeImage {
    pub fn capture_retaining(out:&mut Option<Self>,local:LocalStore,baked:Arc<BakedImageCas>,
        restriction:Arc<dyn RuntimeImageReadRestriction>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"runtime image constructor occupied");
        *out=Some(Self{local,baked,restriction,original,composed:None,pending:None,metadata:BTreeMap::new(),members:BTreeSet::new(),
            index:None,db:None,floor:None,view:None,ready:false});out.as_mut().unwrap().capture()
    }
    fn read(&mut self,r:&ObjectRef)->Result<Vec<u8>> {
        self.original.remaining()?;ensure!(self.pending.is_none(),"original image read unsettled");
        self.composed.as_ref().unwrap().read_object_retaining(&mut self.pending,r.clone(),self.original.clone())?;
        let bytes=self.pending.as_ref().unwrap().verified_bytes()?.to_vec();self.original.remaining()?;self.pending.take();Ok(bytes)
    }
    fn capture(&mut self)->Result<()> {
        self.original.remaining()?;
        ComposedImageStore::capture_retaining(&mut self.composed,self.local.clone(),self.baked.clone(),self.original.clone())?;
        let inventory=self.baked.inventory_data()?;let root=inventory.roots.installer_input;
        let inventory:BTreeSet<_>=inventory.objects.into_iter().collect();let mut pending=vec![root.clone()];let mut bytes=0u64;
        while let Some(r)=pending.pop(){self.original.remaining()?;
            r.validate()?;ensure!(if r.kind==ObjectKind::Blob{r.size_bytes<=4*1024*1024}else{r.size_bytes>0&&r.size_bytes<=64*1024},"runtime content chunk/metadata bound");
            ensure!(inventory.contains(&r),"runtime image edge absent from original baked inventory");
            if !self.members.insert(r.clone()){continue;}ensure!(self.members.len()<=OBJECTS,"runtime image graph exceeds bound");
            if r.kind!=ObjectKind::Blob {ensure!(matches!(r.kind,ObjectKind::Directory|ObjectKind::DirectoryShard|ObjectKind::File),"runtime image noncontent edge");
                bytes=bytes.checked_add(r.size_bytes).context("runtime metadata overflow")?;ensure!(bytes<=METADATA,"runtime image metadata exceeds bound");
                let raw=self.read(&r)?;let edges=crate::store::metadata_references(r.kind,&raw)?;
                if r.kind==ObjectKind::Directory{paged::decode_directory_root(&raw)?;}
                pending.extend(edges);ensure!(pending.len()<=OBJECTS*4,"runtime image frontier exceeds bound");self.metadata.insert(r,raw);
            }
        }
        // Validate every typed metadata edge, including actual shard routing,
        // parent summaries and FileNode logical lengths, not just raw hashes.
        let reader=MetadataReader{bytes:&self.metadata,original:&*self.original};
        for(r,raw)in &self.metadata {self.original.remaining()?;
            if r.kind==ObjectKind::Directory{PagedDirectory::new(&reader).validate_directory(r,OBJECTS as u64)?;}
            for child in crate::store::metadata_references(r.kind,raw)? {
                ensure!(self.members.contains(&child),"runtime image typed edge not in original closure");
                if child.kind!=ObjectKind::Blob {let actual=self.metadata.get(&child).context("runtime metadata child absent")?;
                    match r.kind {ObjectKind::Directory|ObjectKind::DirectoryShard=>paged::validate_child(r.kind,raw,&child,actual)?,
                        ObjectKind::File=>{let parent=decode_file_node(raw)?;let node=decode_file_node(actual)?;
                            for(index,c)in parent.children.iter().enumerate(){if *c==child{validate_file_child(&parent,index,&node)?;}}},_=>bail!("invalid runtime metadata parent")}
                }
            }
        }
        let groups=validate_topology(&reader,&root,&self.members,self.composed.as_ref().unwrap(),&mut self.pending,self.original.clone())?;
        let header=paged::decode_directory_root(self.metadata.get(&root).context("runtime root metadata absent")?)?;
        ensure!(header.entry_count<=i64::MAX as u64&&header.subdirectory_count<=i64::MAX as u64-2,"runtime namespace counts overflow");
        let nonce=Uuid::new_v4().to_string();
        self.floor=Some(Arc::new(Floor{nonce:nonce.clone(),root:root.clone(),composed:self.composed.as_ref().unwrap().clone(),_baked:self.baked.clone(),
            members:self.members.clone(),groups,restriction:self.restriction.clone(),closed:AtomicBool::new(false),requests:Mutex::new((0..REQUESTS).map(|_|None).collect())}));
        self.original.remaining()?;self.index=Some(Connection::open_in_memory()?);self.index.as_ref().unwrap().busy_timeout(Duration::ZERO)?;
        self.db=Some(Connection::open_in_memory()?);let db=self.db.as_ref().unwrap();db.busy_timeout(Duration::ZERO)?;
        db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE nodes(id INTEGER PRIMARY KEY AUTOINCREMENT,kind INTEGER NOT NULL,mode INTEGER NOT NULL,base TEXT,target BLOB NOT NULL,backing TEXT,size INTEGER NOT NULL,nlink INTEGER NOT NULL,parent INTEGER NOT NULL,dirty INTEGER NOT NULL,atime INTEGER NOT NULL,mtime INTEGER NOT NULL,ctime INTEGER NOT NULL,origin TEXT);CREATE TABLE dirents(parent INTEGER NOT NULL,name BLOB NOT NULL,child INTEGER,PRIMARY KEY(parent,name)) WITHOUT ROWID;CREATE TABLE inherited_groups(group_id TEXT PRIMARY KEY,inode INTEGER NOT NULL);")?;
        content::initialize(db)?;namespace::initialize(db,header.entry_count)?;let owner=OwnerEpoch{owner:format!("readonly-image-data:{nonce}"),epoch:1};
        for(k,v)in[("format_version","2".to_owned()),("view_id",nonce.clone()),("base",json_ref(&root)?),("owner",serde_json::to_string(&owner)?),("sealed","true".into())]{db.execute("INSERT INTO meta VALUES(?,?)",params![k,v])?;}
        db.execute("INSERT INTO nodes VALUES(1,2,?,?,?,NULL,0,?,1,0,0,0,0,'')",params![header.mode,json_ref(&root)?,Vec::<u8>::new(),sql_id(header.subdirectory_count+2)])?;
        self.original.remaining()?;
        let manager=VfsManager(Arc::new(Manager{root:PathBuf::new(),store:self.local.clone(),index:Mutex::new(self.index.take().unwrap()),
            verified:Mutex::new((HashMap::new(),VecDeque::new())),remote:None,image:self.floor.clone()}));
        self.view=Some(VfsView(Arc::new(View{manager,id:nonce,owner,path:PathBuf::new(),db:Mutex::new(self.db.take().unwrap()),
            admission:admission::Admission::new(false),_lock:None,mounted:AtomicBool::new(false),next_handle:AtomicU64::new(1),
            handles:Mutex::new(HashMap::new()),pager:None,v2:true}),ReadScope::Plain));
        // Store real owners BEFORE this final late check. No readback/expired
        // constructor can adopt a usable floor after it fails.
        self.original.remaining()?;self.ready=true;self.metadata.clear();Ok(())
    }
    pub fn view(&self)->Result<VfsView>{ensure!(self.ready,"original readonly image floor ACK absent");Ok(self.view.as_ref().unwrap().clone())}
    pub fn installer_root(&self)->Result<&ObjectRef>{ensure!(self.ready,"readonly image floor not ready");Ok(&self.floor.as_ref().unwrap().root)}
    pub fn close_admission(&self){if let Some(f)=&self.floor{f.closed.store(true,Ordering::Release);}}
    /// Retain whole owner until actual Root FUSE/helper/actor/TLS joins. This
    /// immediate count is DATA only and cannot issue any drain/retire receipt.
    pub fn retained_requests_data(&self)->Result<usize>{let f=self.floor.as_ref().context("runtime floor absent")?;
        Ok(f.requests.try_lock().map_err(|_|error(libc::EAGAIN))?.iter().filter(|s|s.is_some()).count())}
    /// Pure ownership transfer only AFTER the enclosing owner has independently
    /// closed and joined its actual FUSE worker. This function cannot prove it.
    pub fn take_retained_request(&self,out:&mut Option<RetainedImageRead>)->Result<bool>{
        ensure!(out.is_none(),"runtime recovery output occupied");let floor=self.floor.as_ref().context("runtime floor absent")?;
        ensure!(floor.closed.load(Ordering::Acquire),"runtime new admission must be closed");
        let mut slots=floor.requests.try_lock().map_err(|_|error(libc::EAGAIN))?;
        let Some(index)=slots.iter().position(|v|v.is_some())else{return Ok(false)};
        let context=slots[index].as_ref().unwrap();ensure!(context.active.load(Ordering::Acquire)==0,"runtime callback still active");
        context.clock.closed.store(true,Ordering::Release);
        *out=Some(RetainedImageRead{context:context.clone(),_floor:floor.clone()});slots[index]=None;Ok(true)
    }
}
pub struct RetainedImageRead{context:Arc<ReadRequest>,_floor:Arc<Floor>}
impl RetainedImageRead {pub fn take_retained_read(&self,out:&mut Option<ComposedRead>)->Result<()>{self.context.session.take_retained_read(out)}}
struct MetadataReader<'a>{bytes:&'a BTreeMap<ObjectRef,Vec<u8>>,original:&'a dyn ImageImportFence}
impl DirectoryStore for MetadataReader<'_>{fn read(&self,r:&ObjectRef)->Result<Vec<u8>>{self.original.remaining()?;Ok(self.bytes.get(r).context("image metadata missing")?.clone())}
    fn put(&self,_:ObjectKind,_:&[u8])->Result<ObjectRef>{Err(error(libc::EROFS))}}
fn validate_topology(reader:&MetadataReader<'_>,root:&ObjectRef,members:&BTreeSet<ObjectRef>,composed:&Arc<ComposedImageStore>,
    pending:&mut Option<ComposedRead>,original:Arc<dyn ImageImportFence>)->Result<BTreeMap<String,(String,u64)>> {
    let tree=PagedDirectory::new(reader);let entry=tree.lookup(root,b"hardlinks.json")?.context("runtime installer must declare hardlinks.json")?;
    let paged::EntryNode::File{object,logical_size,..}=entry.node else{bail!("runtime topology is not a declared file")};
    ensure!(object.kind==ObjectKind::Blob&&object.size_bytes==logical_size&&object.size_bytes<=4*1024*1024&&members.contains(&object),"runtime topology profile differs");
    composed.read_object_retaining(pending,object,original.clone())?;let raw=pending.as_ref().unwrap().verified_bytes()?;
    let topology:HardlinkTopology=serde_json::from_slice(raw)?;
    ensure!(canonical_json(&topology)?==raw&&topology.format_version==1&&topology.groups.len()<=100_000,"invalid runtime hardlink topology");
    let mut groups=BTreeMap::new();let mut previous_group=None;
    for group in topology.groups {original.remaining()?;ensure!((2..=100_000).contains(&group.paths_hex.len()),"runtime link group bound");
        let first=decode_path(&group.paths_hex[0])?;ensure!(previous_group.as_ref().is_none_or(|p|p<&first),"runtime groups unsorted");previous_group=Some(first.clone());
        let group_id=encode_path(&first);let mut previous=None;let mut expected=None;let count=group.paths_hex.len()as u64;
        for raw in group.paths_hex {original.remaining()?;let path=decode_path(&raw)?;ensure!(previous.as_ref().is_none_or(|p|p<&path),"runtime member paths unsorted");previous=Some(path.clone());
            let mut directory=root.clone();let mut file=None;
            for(index,name)in path.iter().enumerate(){let entry=tree.lookup(&directory,name)?.context("runtime hardlink member absent")?;
                match entry.node {paged::EntryNode::Directory{object} if index+1<path.len()=>directory=object,
                    paged::EntryNode::File{object,logical_size,mode} if index+1==path.len()=>{
                        if object.kind==ObjectKind::File{ensure!(decode_file_node(reader.bytes.get(&object).context("runtime hardlink descriptor missing")?)?.logical_size==logical_size,"runtime hardlink size differs");}
                        file=Some((object,logical_size,mode));},_=>bail!("runtime hardlink path must end in a real file")}
            }
            let file=file.context("runtime hardlink member empty")?;if let Some(expected)=&expected{ensure!(*expected==file,"runtime hardlink content/modes differ");}else{expected=Some(file);}
            ensure!(groups.insert(encode_path(&path),(group_id.clone(),count)).is_none()&&groups.len()<=1_000_000,"duplicate/excessive runtime hardlink members");
        }
    }
    original.remaining()?;pending.take();Ok(groups)
}
pub struct RuntimeImageRequest {floor:Arc<Floor>,context:Arc<ReadRequest>,view:VfsView,slot:Option<usize>}
impl RuntimeImageRequest {
    pub fn view(&self)->Result<&VfsView>{self.context.current()?;Ok(&self.view)}
    pub fn invoke<T>(&self,call:impl FnOnce(&VfsView)->Result<T>)->Result<T>{self.context.current()?;
        self.context.active.fetch_add(1,Ordering::AcqRel);let _actual=Active(self.context.clone());self.context.current()?;call(&self.view)}
    /// Actual synchronous callback MUST have returned. Idle is only a read
    /// resource observation, never proof of the original FUSE/worker TLS join.
    pub fn finish_retaining(&mut self)->Result<()> {
        self.context.clock.closed.store(true,Ordering::Release);
        ensure!(self.context.active.load(Ordering::Acquire)==0,"runtime callback still active");
        ensure!(self.context.session.state_data()==ComposedSessionState::Idle,"runtime request retains failed/borrowed read");
        self.context.clock.budget.remaining()?;
        if let Some(index)=self.slot {let mut slots=self.floor.requests.try_lock().map_err(|_|error(libc::EAGAIN))?;
            ensure!(slots[index].as_ref().is_some_and(|v|Arc::ptr_eq(v,&self.context)),"runtime original request slot changed");
            // SAME original clock at the validation point immediately before
            // slot release. Late/cancelled Idle is still retained recovery.
            self.context.clock.budget.remaining()?;
            slots[index]=None;self.slot=None;}Ok(())
    }
}
impl VfsView {
    pub fn begin_readonly_image_request_retaining(&self,out:&mut Option<RuntimeImageRequest>)->Result<bool> {
        ensure!(out.is_none(),"runtime image request output occupied");let Some(floor)=&self.manager().0.image else{return Ok(false)};
        ensure!(matches!(&self.1,ReadScope::Plain),"runtime nested request cannot renew its clock");
        let clock=Arc::new(Clock{budget:FinalizationBudget::begin(Duration::from_secs(30))?,closed:AtomicBool::new(false)});
        let context=Arc::new(ReadRequest{nonce:floor.nonce.clone(),clock:clock.clone(),session:floor.composed.read_session(clock.clone()),active:AtomicUsize::new(0),ready:AtomicBool::new(false)});
        *out=Some(RuntimeImageRequest{floor:floor.clone(),context:context.clone(),view:VfsView(self.0.clone(),ReadScope::Image(context.clone())),slot:None});
        let request=out.as_mut().unwrap();
        loop {clock.remaining()?;match floor.requests.try_lock(){Ok(mut slots)=>{let index=slots.iter().position(|v|v.is_none()).context("runtime request capacity exhausted")?;
                slots[index]=Some(context.clone());request.slot=Some(index);break;},Err(std::sync::TryLockError::WouldBlock)=>std::thread::sleep(clock.remaining()?.min(Duration::from_millis(1))),
                Err(_)=>return Err(error(libc::EIO))}}
        ensure!(!floor.closed.load(Ordering::Acquire),"runtime image new reads closed");floor.restriction.check_new_read(&*clock)?;clock.remaining()?;
        ensure!(!floor.closed.load(Ordering::Acquire),"runtime image admission closed during request");context.ready.store(true,Ordering::Release);Ok(true)
    }
    pub(super) fn image_content(&self,r:&ObjectRef)->Result<Option<&ComposedReadSession>> {
        let Some(floor)=&self.manager().0.image else{return Ok(None)};
        let ReadScope::Image(context)=&self.1 else{return Err(error(libc::EWOULDBLOCK)).context("image read requires actual request scope")};
        context.current()?;ensure!(context.nonce==floor.nonce&&floor.members.contains(r),"runtime image read escaped original closure");Ok(Some(&context.session))
    }
    pub(super) fn image_remaining(&self)->Result<Duration>{
        let ReadScope::Image(context)=&self.1 else{return Err(error(libc::EWOULDBLOCK)).context("image operation requires actual read request")};
        context.current()?;context.clock.remaining()
    }
    pub(super) fn image_group(&self,origin:Option<&str>)->Option<(String,u64)>{
        self.manager().0.image.as_ref().and_then(|f|origin.and_then(|p|f.groups.get(p).cloned()))
    }
    pub(super) fn image_header(&self,r:&ObjectRef,meter:Option<&observation::WorkMeter<'_>>)->Result<(u32,u64,u64)>{
        if let Some(m)=meter{m.metadata(r.size_bytes)?;}let raw=crate::content::ContentStore::read_object(self.image_content(r)?.context("runtime source absent")?,r)?;
        let root=paged::decode_directory_root(&raw)?;Ok((root.mode,root.entry_count,root.subdirectory_count))
    }
    pub(super) fn image_lookup(&self,r:&ObjectRef,name:&[u8],meter:Option<&observation::WorkMeter<'_>>)->Result<Option<paged::DirectoryEntry>>{
        PagedDirectory::new(&ImageDirectory{view:self,meter}).lookup(r,name)
    }
    pub(super) fn image_names(&self,r:&ObjectRef)->Result<Vec<Vec<u8>>>{let adapter=ImageDirectory{view:self,meter:None};let tree=PagedDirectory::new(&adapter);
        let mut names=Vec::new();let mut after=None;loop{let page=tree.scan(r,after.as_deref(),1024)?;
            for e in page.entries{names.push(e.name()?);ensure!(names.len()<=262_144,"runtime directory entries exceed declared image profile");}
            after=page.next_after;if after.is_none(){return Ok(names)}}}
}
struct ImageDirectory<'a,'b>{view:&'a VfsView,meter:Option<&'a observation::WorkMeter<'b>>}
impl DirectoryStore for ImageDirectory<'_,'_>{fn read(&self,r:&ObjectRef)->Result<Vec<u8>>{
    if let Some(m)=self.meter{m.metadata(r.size_bytes)?;}DirectoryStore::read(self.view.image_content(r)?.context("runtime image content absent")?,r)}
    fn put(&self,_:ObjectKind,_:&[u8])->Result<ObjectRef>{Err(error(libc::EROFS))}}

#[cfg(test)]
#[path="runtime_image_tests.rs"]
mod tests;
