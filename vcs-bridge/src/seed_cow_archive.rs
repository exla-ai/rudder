//! Byte builders for the LIVE shared-object alternate and CLOSED independent
//! history graph. These mint no project/task/readiness or actor-join authority.
use super::*;
use rudder_engine::{directory::{DirectoryEntry,EntryNode,LocalDirectoryStore,PagedDirectory},
    execution::finalization_budget::FinalizationBudget,store::LocalStore,
    vfs::linux::metadata::MetadataCowCheckpoint};
use std::sync::atomic::{AtomicBool,Ordering};
fn time(b:&FinalizationBudget,c:&AtomicBool)->Result<()>{if c.load(Ordering::Acquire){b.cancel();anyhow::bail!("original CoW graph job cancelled");}b.remaining()?;Ok(())}
fn blob(store:&LocalStore,stage:&str,b:&[u8])->Result<ObjectRef>{let r=ObjectRef::from_bytes(ObjectKind::Blob,b);store.receive_staged(stage,&r,b)?;Ok(r)}
fn child(store:&LocalStore,root:&ObjectRef,name:&[u8])->Result<ObjectRef>{let e=rudder_engine::tree::lookup(store,root,name)?.context("CoW graph directory absent")?;
    let EntryNode::Directory{object}=e.node else{anyhow::bail!("CoW graph path is not a Directory")};Ok(object)}
/// Acknowledged immutable floor DATA; Root's actual mount/IPC owners remain
/// separate. Stage this carrier before any CAS write or original pin intent.
pub struct CowMetadataFloor {store:LocalStore,source:ObjectRef,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    stage:Option<String>,root:Option<ObjectRef>,pin:String,attempted:bool,pin_attempted:bool,ack:bool}
impl CowMetadataFloor {
    pub fn prepare_retaining(out:&mut Option<Self>,store:LocalStore,source:ObjectRef,objects_path:&Path,pin:String,
        original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original CoW floor owner occupied");*out=Some(Self{store,source,original:original.clone(),cancelled,
            stage:None,root:None,pin,attempted:false,pin_attempted:false,ack:false});out.as_mut().unwrap().prepare(objects_path)
    }
    fn prepare(&mut self,objects_path:&Path)->Result<()> {
        ensure!(!self.attempted,"CoW floor cannot replay");self.attempted=true;time(&self.original,&self.cancelled)?;
        let(descriptor,_)=paged::read_paged_descriptor(&self.store,&self.source,||time(&self.original,&self.cancelled))?;
        let text=objects_path.to_str().context("CoW alternate path not UTF8")?;
        ensure!(objects_path.is_absolute()&&text.len()<4096&&!text.contains(['\n','\r'])&&self.store.retention()?.pins.values().any(|r|r==&self.source),"CoW floor original input/path invalid");
        self.stage=Some(self.store.begin_staging(std::time::Duration::from_secs(120))?.id);let stage=self.stage.as_ref().unwrap();
        let adapter=LocalDirectoryStore::new(&self.store,stage);let dirs=PagedDirectory::new(&adapter);
        let alt=blob(&self.store,stage,format!("{text}\n").as_bytes())?;
        let info=dirs.build(0o700,[DirectoryEntry::new(b"alternates",EntryNode::File{logical_size:alt.size_bytes,object:alt,mode:0o600})?])?;
        let objects=dirs.build(0o700,[DirectoryEntry::new(b"info",EntryNode::Directory{object:info})?])?;
        self.root=Some(paged::private_root(&self.store,&descriptor.metadata,&objects,Some(stage))?);
        time(&self.original,&self.cancelled)?;self.pin_attempted=true;
        self.store.publish_extending(stage,&self.pin,self.root.as_ref().unwrap(),&[self.source.clone()])?;
        self.ack=true;time(&self.original,&self.cancelled)
    }
    pub fn root(&self)->Result<&ObjectRef>{ensure!(self.ack,"original CoW floor pin ACK absent");self.root.as_ref().context("CoW floor root absent")}
}
/// Actual pin ACK for the independent full private history tree. Root also
/// retains the ORIGINAL MetadataCowCheckpoint with its genuine FUSE/TLS join.
pub struct CowArchiveLocalization {store:LocalStore,source:ObjectRef,raw:ObjectRef,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    stage:Option<String>,root:Option<ObjectRef>,hardlinks:ObjectRef,pin:String,attempted:bool,pin_attempted:bool,ack:bool}
impl CowArchiveLocalization {
    pub fn prepare_retaining(out:&mut Option<Self>,store:LocalStore,source:ObjectRef,actual:&MetadataCowCheckpoint,objects_path:&Path,
        pin:String,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original CoW archive localization occupied");actual.check_retained()?;
        *out=Some(Self{store,source,raw:actual.snapshot().directory.clone(),original:original.clone(),cancelled,stage:None,root:None,hardlinks:actual.snapshot().hardlinks.clone(),pin,attempted:false,pin_attempted:false,ack:false});
        out.as_mut().unwrap().prepare(objects_path)
    }
    fn prepare(&mut self,objects_path:&Path)->Result<()> {
        ensure!(!self.attempted,"unknown CoW archive publication cannot replay");self.attempted=true;time(&self.original,&self.cancelled)?;
        let(descriptor,_)=paged::read_paged_descriptor(&self.store,&self.source,||time(&self.original,&self.cancelled))?;
        let pins=self.store.retention()?;for r in [&self.source,&self.raw]{ensure!(pins.pins.values().any(|p|p==r),"CoW archive source/raw checkpoint retention absent");}
        self.stage=Some(self.store.begin_staging(std::time::Duration::from_secs(120))?.id);let stage=self.stage.as_ref().unwrap();
        let adapter=LocalDirectoryStore::new(&self.store,stage);let dirs=PagedDirectory::new(&adapter);
        self.root=Some(localize_private_tree(&self.store,&dirs,&descriptor.objects,&self.raw,objects_path,&self.original,&self.cancelled)?);
        rudder_engine::vfs::validate_snapshot(&self.store,&rudder_engine::vfs::VfsCheckpoint{format_version:1,directory:self.root.as_ref().unwrap().clone(),hardlinks:self.hardlinks.clone()})?;
        time(&self.original,&self.cancelled)?;self.pin_attempted=true;
        self.store.publish_extending(stage,&self.pin,self.root.as_ref().unwrap(),&[self.raw.clone(),self.source.clone()])?;
        self.ack=true;time(&self.original,&self.cancelled)
    }
    pub fn root(&self)->Result<&ObjectRef>{ensure!(self.ack,"original independent CoW history pin ACK absent");self.root.as_ref().context("independent CoW history root absent")}
    pub fn check_retained(&self)->Result<()>{ensure!(self.ack&&self.store.retention()?.pins.get(&self.pin)==self.root.as_ref(),"original CoW history pin lost");Ok(())}
}
/// Pure Merkle byte operation. This emits DATA only, never a join, pin ACK,
/// Root retirement or project grant. The retained wrapper above supplies the
/// actual original checkpoint owner and keeps the publication intent.
pub(super) fn localize_private_tree<S:rudder_engine::directory::DirectoryStore>(store:&LocalStore,dirs:&PagedDirectory<'_,S>,base_objects:&ObjectRef,
    raw:&ObjectRef,objects_path:&Path,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<ObjectRef>{
    time(original,cancelled)?;
        let git=child(store,raw,b".git")?;let objects=child(store,&git,b"objects")?;
        let mut upper=objects.clone();
        if let Some(entry)=dirs.lookup(&objects,b"info")?{
            let EntryNode::Directory{object:info}=entry.node else{anyhow::bail!("CoW objects info is not directory")};
            if let Some(entry)=dirs.lookup(&info,b"alternates")?{
                let EntryNode::File{object,logical_size,..}=entry.node else{anyhow::bail!("CoW alternate not a file")};
                ensure!(logical_size<=4096,"CoW alternate exceeds bound");let bytes=rudder_engine::content::read_file_range(store,&object,0,logical_size as usize)?;
                ensure!(bytes==format!("{}\n",objects_path.to_str().context("CoW expected path not UTF8")?).as_bytes(),"CoW archive refuses foreign/changed alternates");
                let info=dirs.update(&info,b"alternates",None)?.root;
                upper=dirs.update(&upper,b"info",Some(EntryNode::Directory{object:info}))?.root;
            }
        }
        let mut count=0;let mut logical=0;
        let merged=merge_objects(store,dirs,base_objects,&upper,0,&mut count,&mut logical,original,cancelled)?;
        let git=dirs.update(&git,b"objects",Some(EntryNode::Directory{object:merged}))?.root;
        let root=dirs.update(raw,b".git",Some(EntryNode::Directory{object:git}))?.root;
    time(original,cancelled)?;Ok(root)
}
/// Visit only PRIVATE additions. Original immutable directory pages and file
/// refs are retained/shared. Conflicting object filenames fail, never replace
/// original history. No source-pack reads, copies or per-agent hydration.
fn merge_objects<S:rudder_engine::directory::DirectoryStore>(store:&LocalStore,dirs:&PagedDirectory<'_,S>,base:&ObjectRef,upper:&ObjectRef,depth:usize,
    count:&mut usize,logical:&mut u64,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<ObjectRef>{
    time(original,cancelled)?;ensure!(depth<=32,"CoW object archive depth exceeded");let mut root=base.clone();let mut after=None;
    loop{let page=dirs.scan(upper,after.as_deref(),128)?;for entry in page.entries{
        *count+=1;ensure!(*count<=paged::V2_MAX_OBJECT_FILES,"private object archive exceeds bounded entry profile");let name=entry.name()?;
        let old=dirs.lookup(&root,&name)?;
        let node=match(entry.node,old.map(|e|e.node)){
            (EntryNode::Directory{object},old)=>{let base=match old{Some(EntryNode::Directory{object})=>object,None=>dirs.build(0o700,[])?,_=>anyhow::bail!("private object path conflicts with original history")};
                EntryNode::Directory{object:merge_objects(store,dirs,&base,&object,depth+1,count,logical,original,cancelled)?}},
            (new@EntryNode::File{logical_size,..},old)=>{*logical=logical.checked_add(logical_size).context("private object size overflow")?;
                ensure!(logical_size<=paged::V2_MAX_FILE_BYTES&&*logical<=paged::V2_MAX_OBJECT_BYTES,"private object archive exceeds byte profile");
                if let Some(old)=old{ensure!(matches!((&old,&new),(EntryNode::File{object:a,logical_size:x,..},EntryNode::File{object:b,logical_size:y,..})if a==b&&x==y),"private object filename conflicts with original retained history");new}else{new}},
            _=>anyhow::bail!("unsupported private object archive entry"),
        };
        root=dirs.update(&root,&name,Some(node))?.root;time(original,cancelled)?;
    }after=page.next_after;if after.is_none(){break;}}
    Ok(root)
}
