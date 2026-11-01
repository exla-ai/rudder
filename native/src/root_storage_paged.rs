//! V2 Root data plane. Root-owned persistent CAS + paged private metadata VFS.
//! Called only by the original credential-checked storage endpoint dispatcher.
//! No public path/JSON constructor, alternate host socket or new scheduler.
use super::*;
use anyhow::bail;
use std::sync::Arc;
use rudder_engine::{vfs::{VfsManager,linux::metadata::{RootMetadataNamespace,RetainedMetadataMount,MetadataMountMode}},
    protocol::OwnerEpoch};
use rudder_vcs_bridge::{AttemptBinding,seed::{read_paged_seed,PagedSeedDescriptor,CowMetadataFloor,CowArchiveLocalization,paged::*}};
const DISK_ROOT:&str="/workspace/.rudder-seeds-v2";
const PUBLIC_ROOT:&str="/run/rudder-managed-seeds";
const MAX_ARCHIVES:usize=4096; // bounded retained actual owners; no automatic history GC.

pub(crate) fn bound(reference:&ObjectRef,remaining:u64)->Result<()> {
    reference.validate()?;ensure!(matches!(reference.kind,ObjectKind::Directory|ObjectKind::DirectoryShard|ObjectKind::File|ObjectKind::Blob)
        &&reference.size_bytes<=remaining&&reference.size_bytes<=4<<20&&match reference.kind{
            ObjectKind::Directory=>reference.size_bytes<=4096,ObjectKind::File=>reference.size_bytes<=64<<10,
            ObjectKind::DirectoryShard=>reference.size_bytes<=128<<10,_=>true},"paged seed object exceeds V2 profile");Ok(())
}
fn admit(root:&ObjectRef,refs:usize,bytes:u64)->Result<()>{bound(root,V2_MAX_CAS_BYTES)?;
    ensure!(root.kind==ObjectKind::Directory&&refs>0&&refs<=V2_MAX_REFS&&bytes>0&&bytes<=V2_MAX_CAS_BYTES,"paged seed closure exceeds V2 profile");Ok(())}
pub(super) fn archive_capacity_available(archived:usize,active:impl IntoIterator<Item=usize>)->bool {
    // Every retained cow, including partial/unknown creation, already owns its
    // eventual archive slot. Release moves that owner without creating space.
    active.into_iter().try_fold(archived,usize::checked_add).is_some_and(|used|used<MAX_ARCHIVES)
}
fn protected_parent(path:&Path)->Result<()> {
    ensure!(path.is_absolute()&&path.canonicalize()?==path,"paged directory ancestry redirects");
    for p in path.ancestors(){let m=fs::symlink_metadata(p)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"paged ancestry is not Root protected");}Ok(())
}
struct HeldDirectory {path:PathBuf,file:File,id:(u64,u64),mode:u32}
impl HeldDirectory {fn capture(path:&Path,mode:u32)->Result<Self>{protected_parent(path.parent().context("paged directory parent absent")?)?;let file=open_root_directory(path)?;let m=file.metadata()?;
    let owner=Self{path:path.into(),file,id:(m.dev(),m.ino()),mode};owner.check()?;Ok(owner)}
    fn check(&self)->Result<()>{protected_parent(self.path.parent().context("paged directory parent absent")?)?;for m in [self.file.metadata()?,fs::symlink_metadata(&self.path)?]{ensure!(m.is_dir()&&!m.file_type().is_symlink()
        &&(m.dev(),m.ino())==self.id&&m.uid()==0&&m.gid()==0&&m.mode()&0o7777==self.mode,"paged original directory changed");}Ok(())}}
fn directory(path:&Path,mode:u32)->Result<HeldDirectory>{protected_parent(path.parent().context("paged new directory parent absent")?)?;fs::DirBuilder::new().mode(mode).create(path)?;
    fs::set_permissions(path,fs::Permissions::from_mode(mode))?;HeldDirectory::capture(path,mode)}
fn volume_id(path:&Path)->Result<u64>{let mut found=None;for line in String::from_utf8(proc_bytes("/proc/thread-self/mountinfo",1<<20)?)?.lines(){
    let(l,_)=line.split_once(" - ").context("paged volume mountinfo incomplete")?;let p=l.split(' ').collect::<Vec<_>>();
    ensure!(p.len()>=6,"paged volume mountinfo malformed");if p[4]==path.to_str().unwrap(){ensure!(found.is_none(),"paged volume mount ambiguous");found=Some(p[0].parse()?);}}
    found.context("paged storage requires actual mounted volume")}
struct Volume {directory:File,id:(u64,u64),mount_id:u64,root_mount_id:u64}
impl Volume {
    fn capture()->Result<Self>{let p=Path::new("/workspace");let directory=open_root_directory(p)?;let m=directory.metadata()?;
        ensure!(m.uid()==0&&m.gid()==0&&m.mode()&0o022==0&&p.canonicalize()?==p,"paged volume is not Root protected");
        let value=Self{directory,id:(m.dev(),m.ino()),mount_id:volume_id(p)?,root_mount_id:volume_id(Path::new("/"))?};value.current()?;Ok(value)}
    fn current(&self)->Result<()>{let a=self.directory.metadata()?;let b=fs::symlink_metadata("/workspace")?;
        ensure!((a.dev(),a.ino())==self.id&&(b.dev(),b.ino())==self.id&&b.uid()==0&&b.gid()==0&&b.mode()&0o022==0
            &&volume_id(Path::new("/workspace"))?==self.mount_id&&self.mount_id!=self.root_mount_id,"paged original volume changed/fallback image root");Ok(())}
}
fn open_root_directory(path:&Path)->Result<File>{Ok(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?)}
struct Export {manager:Option<VfsManager>,mount:Option<RetainedMetadataMount>,root:PathBuf,original_pin:String,pin_attempted:bool}
struct Cow {localized:Option<CowArchiveLocalization>,cancel:Option<Cancellation>,checkpoint:Option<rudder_engine::vfs::linux::metadata::MetadataCowCheckpoint>,binding:AttemptBinding,root:PathBuf,nonce:String,manager:Option<VfsManager>,mount:Option<RetainedMetadataMount>,
    attempted:bool,ready:bool,helper:Option<uuid::Uuid>,archive:Option<ArchiveIntent>}
struct ArchiveIntent {request:String,original:FinalizationBudget,cancel:Option<Cancellation>,ack:bool}
struct PagedSlot {announced:BTreeSet<ObjectRef>,announced_bytes:u64,partial:BTreeMap<ObjectRef,Vec<u8>>,missing:BTreeSet<ObjectRef>,seed:ObjectRef,refs:usize,bytes:u64,received:BTreeSet<ObjectRef>,received_bytes:u64,pending:Option<(ObjectRef,Vec<u8>)>,
    stage:Option<String>,import_pin:String,pin_attempted:bool,budget:FinalizationBudget,cancel:Cancellation,
    descriptor:Option<PagedSeedDescriptor>,cow_manager:Option<VfsManager>,floor:Option<CowMetadataFloor>,export:Export,cows:BTreeMap<String,Cow>,helpers:BTreeMap<uuid::Uuid,Process>,
    ready:bool,closed:bool,retirement:Option<(uuid::Uuid,FinalizationBudget)>,retired:bool,import_removed:bool}
/// Retains every partially created directory/mount/worker before effects. A
/// failed initialization cannot create a second owner or reopen saved paths.
pub(super) struct RootPagedStorage {
    volume:Volume,namespace:Arc<RootMetadataNamespace>,nonce:String,disk:Option<HeldDirectory>,public_anchor:Option<HeldDirectory>,public:Option<HeldDirectory>,
    store:Option<LocalStore>,store_dir:Option<HeldDirectory>,initialized:bool,attempted:bool,slots:BTreeMap<uuid::Uuid,PagedSlot>,archives:BTreeMap<String,Cow>,
}
impl RootPagedStorage {
    pub(super) fn capture_retaining(out:&mut Option<Self>)->Result<()> {ensure!(out.is_none(),"paged Root storage owner already staged");
        let volume=Volume::capture()?;let namespace=RootMetadataNamespace::capture()?;
        *out=Some(Self{volume,namespace,nonce:uuid::Uuid::new_v4().to_string(),disk:None,public_anchor:None,public:None,store:None,store_dir:None,initialized:false,attempted:false,
            slots:BTreeMap::new(),archives:BTreeMap::new()});Ok(())}
    fn initialize(&mut self,original:&FinalizationBudget)->Result<()> {
        ensure!(!self.attempted,"paged storage initialization cannot replay");self.attempted=true;self.volume.current()?;original.remaining()?;
        // An existing process/boot's root requires explicit recovery. No chmod,
        // recursive chown, old-path adoption or mounting over user checkouts.
        self.disk=Some(directory(Path::new(DISK_ROOT),0o700)?);
        fs::DirBuilder::new().mode(0o700).create(Path::new(DISK_ROOT).join("managers"))?;
        self.public_anchor=Some(match fs::symlink_metadata(PUBLIC_ROOT){
            Ok(_)=>HeldDirectory::capture(Path::new(PUBLIC_ROOT),0o711)?,
            Err(error)if error.kind()==std::io::ErrorKind::NotFound=>directory(Path::new(PUBLIC_ROOT),0o711)?,Err(error)=>return Err(error.into())});
        let public=Path::new(PUBLIC_ROOT).join(&self.nonce);self.public=Some(directory(&public,0o711)?);
        for name in ["seeds","forks"]{let _=directory(&public.join(name),0o711)?;}
        self.store=Some(LocalStore::open(Path::new(DISK_ROOT).join("cas"))?);self.store_dir=Some(HeldDirectory::capture(self.store.as_ref().unwrap().root(),0o700)?);self.current()?;original.remaining()?;self.initialized=true;Ok(())
    }
    fn current(&self)->Result<()> {self.volume.current()?;self.namespace.check_current()?;
        if let Some(d)=&self.disk{d.check()?;}if let Some(p)=&self.public_anchor{p.check()?;}if let Some(p)=&self.public{p.check()?;}if let Some(cas)=&self.store_dir{cas.check()?;}Ok(())}
    pub(super) fn contains(&self,id:&uuid::Uuid)->bool{self.slots.contains_key(id)}
    pub(super) fn slots(&self)->usize{self.slots.len()}
    pub(super) fn close_admission(&mut self){for slot in self.slots.values_mut(){slot.closed=true;if let Some(m)=&slot.export.mount{m.close_admission();}}}
    fn store(&self)->Result<&LocalStore>{self.store.as_ref().context("paged Root CAS absent")}
    pub(super) fn dispatch(&mut self,request:Request,payload:&[u8],fd:Option<OwnedFd>,constraint:&FinalizationConstraint,
        engine:&Process,engine_image:&Image)->Result<Reply>{
        self.current()?;
        if let Request::BeginPaged {constraint:original,seed,refs,bytes}=request{
            ensure!(&original==constraint&&payload.is_empty()&&self.slots.len()<MAX_SEEDS,"paged seed admission unavailable");admit(&seed,refs,bytes)?;
            let budget=FinalizationBudget::from_constraint(&original)?;
            if !self.initialized{self.initialize(&budget)?;}
            let cancel=Cancellation::capture(fd.context("paged original cancel FD absent")?)?;let id=uuid::Uuid::new_v4();
            let public=self.public.as_ref().unwrap().path.join("seeds").join(id.to_string());
            self.slots.insert(id,PagedSlot{announced:BTreeSet::new(),announced_bytes:0,partial:BTreeMap::new(),missing:BTreeSet::new(),seed,refs,bytes,received:BTreeSet::new(),received_bytes:0,pending:None,stage:None,
                import_pin:format!("root-paged-import:{id}"),pin_attempted:false,budget,cancel,descriptor:None,cow_manager:None,floor:None,
                export:Export{manager:None,mount:None,root:public.join("seed"),original_pin:format!("root-paged-view:{id}"),pin_attempted:false},
                cows:BTreeMap::new(),helpers:BTreeMap::new(),ready:false,closed:false,retirement:None,retired:false,import_removed:false});
            let store=self.store()?.clone();let slot=self.slots.get_mut(&id).unwrap();slot.cancel.start(slot.budget.clone())?;
            let _=directory(slot.export.root.parent().unwrap(),0o711)?;
            slot.stage=Some(store.begin_staging(Duration::from_secs(120))?.id);slot.budget.remaining()?;return Ok(Reply::Begun{slot:id})
        }
        let id=match &request{Request::Object{slot,..}|Request::PagedInventory{slot,..}|Request::PagedChunk{slot,..}|Request::Chunk{slot,..}|Request::Commit{slot}|Request::Check{slot}|Request::Fence{slot}
            |Request::EnrollHelper{slot}|Request::EnrollHelperCow{slot,..}|Request::CreateMetadataCow{slot,..}|Request::ArchiveMetadataCow{slot,..}
            |Request::RetireSeed{slot,..}|Request::ReleaseSeed{slot,..}=>*slot,_=>bail!("unsupported paged Root request")};
        let store=self.store()?.clone();let namespace=self.namespace.clone();let public=self.public.as_ref().unwrap().path.clone();
        let archive_capacity=archive_capacity_available(self.archives.len(),self.slots.values().map(|slot|slot.cows.len()));
        let slot=self.slots.get_mut(&id).context("original paged slot absent")?;
        if let Request::Fence{..}=request{slot.closed=true;if let Some(m)=&slot.export.mount{m.close_admission();}return Ok(Reply::Accepted)}
        if let Request::Check{..}=request{ensure!(slot.ready&&!slot.closed,"paged seed closed/not ready");slot.export.mount.as_ref().context("paged export absent")?.check_current()?;return Ok(Reply::Accepted)}
        if let Request::CreateMetadataCow{nonce,binding,..}=request{
            ensure!(payload.is_empty()&&fd.is_some()&&slot.ready&&!slot.closed&&slot.cows.len()<MAX_HELPERS_PER_SEED&&archive_capacity,"paged CoW admission unavailable");
            binding.validate()?;ensure!(binding.seed==slot.seed&&binding.baseline==slot.descriptor.as_ref().unwrap().baseline
                &&binding.baseline_operation==slot.descriptor.as_ref().unwrap().operation&&uuid::Uuid::parse_str(&nonce)?.to_string()==nonce&&!slot.cows.contains_key(&nonce),"paged CoW binding differs");
            let original=FinalizationBudget::from_constraint(constraint)?;original.remaining()?;
            let root=public.join("forks").join(&nonce).join("private");
            slot.cows.insert(nonce.clone(),Cow{localized:None,cancel:None,checkpoint:None,binding,root:root.clone(),nonce:nonce.clone(),manager:None,mount:None,attempted:false,ready:false,helper:None,archive:None});
            let cow=slot.cows.get_mut(&nonce).unwrap();cow.attempted=true;
            cow.cancel=Some(Cancellation::capture(fd.context("original CoW cancel FD absent")?)?);cow.cancel.as_mut().unwrap().start(original.clone())?;
            let _=directory(root.parent().unwrap(),0o711)?;let _=directory(&root,0o700)?;
            cow.manager=Some(slot.cow_manager.as_ref().context("original prepared CoW floor absent")?.clone());
            let manager=cow.manager.as_ref().unwrap();let base=slot.floor.as_ref().context("actual CoW floor absent")?.root()?.clone();original.remaining()?;
            let view=manager.fork_paged(&nonce,&base,cow.binding.owner.clone())?;
            RetainedMetadataMount::capture_retaining(&mut cow.mount,namespace,view,root.clone(),MetadataMountMode::PrivateCow,nonce.clone())?;
            cow.mount.as_mut().unwrap().mount_retaining(&original,&cow.cancel.as_ref().unwrap().flag)?;
            cow.cancel.as_mut().unwrap().finish()?;
            cow.cancel.as_ref().unwrap().check_success(&original)?;cow.ready=true;original.remaining()?;return Ok(Reply::MetadataCowReady{slot:id,nonce,root,base})
        }
        if let Request::ArchiveMetadataCow{nonce,request,..}=request{
            ensure!(payload.is_empty()&&fd.is_some(),"paged archive requires actual cleanup cancellation");
            let original=FinalizationBudget::from_constraint(constraint)?;let cow=slot.cows.get_mut(&nonce).context("original CoW absent")?;
            if let Some(reader)=cow.helper{ensure!(slot.helpers.get(&reader).context("original helper lost")?.exited()?,"original CoW helper still live");}
            if let Some(old)=&cow.archive{ensure!(old.request==request&&old.original.constraint()==*constraint,"CoW archive request/budget replaced");}
            else{original.remaining()?;cow.archive=Some(ArchiveIntent{request:request.clone(),original:original.clone(),cancel:None,ack:false});
                let intent=cow.archive.as_mut().unwrap();intent.cancel=Some(Cancellation::capture(fd.context("original archive cancel FD absent")?)?);intent.cancel.as_mut().unwrap().start(original.clone())?;}
            let mount=cow.mount.as_mut().context("original CoW mount absent; partial constructor retained")?;
            // Native's fixed typed issuer proves actor/factory/ODB quiescence.
            // Root additionally proves actual helper exit, normal umount and
            // original FUSE server/TLS join; request DATA never substitutes.
            loop{original.remaining()?;match mount.unmount_and_checkpoint_retaining(&original,&cow.archive.as_ref().unwrap().cancel.as_ref().unwrap().flag){
                Ok(true)=>break,Ok(false)=>{},Err(error)=>{if mount.is_unmounted_and_joined()&&mount.checkpoint().is_ok(){break}return Err(error)}
            }std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
            mount.take_checkpoint_retaining(&mut cow.checkpoint)?;
            // Kernel sharing while LIVE uses the actual common RO alternate.
            // Only after genuine normal umount+TLS join do we graft original
            // object refs into the independent private archive. No byte copy.
            let result=CowArchiveLocalization::prepare_retaining(&mut cow.localized,store.clone(),slot.seed.clone(),cow.checkpoint.as_ref().unwrap(),
                &slot.export.root.join("objects"),format!("root-cow-archive:{nonce}"),&original,cow.archive.as_ref().unwrap().cancel.as_ref().unwrap().flag.clone());
            if let Err(error)=result{if !cow.localized.as_ref().is_some_and(|a|a.root().is_ok()&&a.check_retained().is_ok()){return Err(error)}}
            let directory=cow.localized.as_ref().unwrap().root()?.clone();
            let hardlinks=cow.checkpoint.as_ref().unwrap().snapshot().hardlinks.clone();cow.archive.as_mut().unwrap().cancel.as_mut().unwrap().finish()?;cow.archive.as_mut().unwrap().ack=true;
            cow.mount=None;cow.manager=None; // Actual mutable DB/namespace FD closure is OFF App.
            return Ok(Reply::MetadataCowArchived{slot:id,nonce,request,directory,hardlinks})
        }
        if let Request::EnrollHelperCow{nonce,..}=request{
            let cow=slot.cows.get(&nonce).context("original CoW enrollment absent")?;
            ensure!(cow.ready&&cow.archive.is_none()&&cow.helper.is_none(),"original CoW enrollment closed/already bound");
            ensure!(slot.ready&&!slot.closed&&slot.helpers.len()<MAX_HELPERS_PER_SEED,"paged helper admission closed/full");
            let helper=Process::capture(fd.context("paged actual original helper pidfd absent")?)?;
            engine.live()?;uid_identity(helper.pid,1500)?;engine_image.matches_process(&helper)?;
            let status=String::from_utf8(proc_bytes(&format!("/proc/{}/status",helper.pid),16384)?)?;
            ensure!(status.lines().find_map(|l|l.strip_prefix("PPid:")).context("paged helper parent absent")?.trim().parse::<u32>()?==engine.pid,"paged helper is not original engine child");helper.live()?;
            let reader=uuid::Uuid::new_v4();let pid=helper.pid;slot.helpers.insert(reader,helper);
            slot.cows.get_mut(&nonce).unwrap().helper=Some(reader);return Ok(Reply::HelperEnrolled{reader,pid})
        }
        if let Request::RetireSeed{retirement,readers,..}=request{
            ensure!(payload.is_empty()&&fd.is_none()&&slot.ready,"invalid paged retirement");let mut expected=BTreeSet::new();
            for r in readers{ensure!(expected.insert(uuid::Uuid::parse_str(&r)?),"duplicate paged reader");}
            ensure!(slot.helpers.keys().copied().collect::<BTreeSet<_>>()==expected,"paged full helper set differs");
            for h in slot.helpers.values(){ensure!(h.exited()?,"paged sibling helper still live");}
            ensure!(slot.cows.values().all(|c|c.archive.as_ref().is_some_and(|a|a.ack)&&c.mount.is_none()&&c.checkpoint.is_some()&&c.localized.is_some()),"paged native metadata/history readers still held");
            for cow in slot.cows.values(){cow.checkpoint.as_ref().unwrap().check_retained()?;cow.localized.as_ref().unwrap().check_retained()?;}
            let original=FinalizationBudget::from_constraint(constraint)?;
            if let Some((id,old))=&slot.retirement{ensure!(*id==retirement&&old.constraint()==*constraint,"paged retirement original replaced");}
            else{slot.closed=true;slot.retirement=Some((retirement,original.clone()));}
            let mount=slot.export.mount.as_mut().context("paged original export absent")?;mount.close_admission();
            loop{original.remaining()?;if mount.unmount_and_checkpoint_retaining(&original,&slot.cancel.flag)?{break}
                std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
            if !slot.import_removed{ensure!(store.retention()?.pins.get(&slot.import_pin)==Some(&slot.seed),"paged import pin changed");
                store.unpin(&slot.import_pin)?;slot.import_removed=true;}
            slot.retired=true;original.remaining()?;return Ok(Reply::SeedRetired{slot:id,retirement})
        }
        if let Request::ReleaseSeed{retirement,..}=request{
            ensure!(slot.retired&&slot.retirement.as_ref().is_some_and(|(r,_)|*r==retirement),"paged retirement ACK absent");
            let removed=self.slots.remove(&id).unwrap();for(nonce,cow)in removed.cows{ensure!(self.archives.insert(nonce,cow).is_none(),"paged archive collision");}
            // Shared disk CAS/history pins are retained independently of this
            // reusable active seed slot. No source/history deletion occurs.
            return Ok(Reply::SeedReleased{slot:id,retirement})
        }
        ensure!(!slot.ready&&!slot.closed&&slot.budget.constraint()==*constraint,"paged preparation clock/state differs");slot.budget.remaining()?;
        match request{
            Request::PagedInventory{references,..}=>{
                ensure!(payload.is_empty()&&!references.is_empty()&&references.len()<=16,"V2 inventory batch exceeds profile");let mut missing=Vec::new();
                for(index,reference)in references.into_iter().enumerate(){bound(&reference,slot.bytes-slot.announced_bytes)?;
                    ensure!(slot.announced.len()<slot.refs&&slot.announced.insert(reference.clone()),"V2 original object repeated/excess");slot.announced_bytes+=reference.size_bytes;
                    if let Ok(bytes)=store.read_object(&reference){reference.verify(&bytes)?;store.receive_staged(slot.stage.as_ref().unwrap(),&reference,&bytes)?;
                        slot.received_bytes+=reference.size_bytes;slot.received.insert(reference);
                    }else{slot.missing.insert(reference);missing.push(index);}
                }slot.budget.remaining()?;return Ok(Reply::PagedMissing{slot:id,missing})
            }
            Request::PagedChunk{reference,offset,..}=>{
                ensure!(slot.missing.contains(&reference)&&payload.len()<=CHUNK&&(!payload.is_empty()||reference.size_bytes==0),"V2 chunk outside actual missing inventory");
                if !slot.partial.contains_key(&reference){ensure!(offset==0&&slot.partial.len()<8,"V2 partial object admission full/order invalid");slot.partial.insert(reference.clone(),Vec::with_capacity(reference.size_bytes.try_into()?));}
                let bytes=slot.partial.get_mut(&reference).unwrap();ensure!(offset==bytes.len()&&bytes.len()+payload.len()<=reference.size_bytes as usize,"V2 chunk order/size differs");bytes.extend_from_slice(payload);
                if bytes.len()as u64==reference.size_bytes{store.receive_staged(slot.stage.as_ref().unwrap(),&reference,bytes)?;slot.partial.remove(&reference);slot.missing.remove(&reference);
                    slot.received_bytes+=reference.size_bytes;slot.received.insert(reference);}
            }
            Request::Object{reference,..}=>{bound(&reference,slot.bytes-slot.received_bytes)?;
                ensure!(payload.is_empty()&&slot.pending.is_none()&&slot.received.len()<slot.refs&&!slot.received.contains(&reference),"paged object order exceeded");
                // Existing immutable bytes are trusted only after actual hash
                // verification in the held Root CAS. No caller present flag.
                if let Ok(bytes)=store.read_object(&reference){reference.verify(&bytes)?;store.receive_staged(slot.stage.as_ref().unwrap(),&reference,&bytes)?;
                    slot.received_bytes+=reference.size_bytes;slot.received.insert(reference);return Ok(Reply::ObjectPresent)}
                slot.pending=Some((reference.clone(),Vec::with_capacity(reference.size_bytes.try_into()?)));}
            Request::Chunk{offset,..}=>{let(r,bytes)=slot.pending.as_mut().context("paged object not started")?;
                ensure!(offset==bytes.len()&&payload.len()<=CHUNK&&bytes.len()+payload.len()<=r.size_bytes as usize&&(!payload.is_empty()||r.size_bytes==0),"paged chunk order/size differs");
                bytes.extend_from_slice(payload);if bytes.len()as u64==r.size_bytes{store.receive_staged(slot.stage.as_ref().unwrap(),r,bytes)?;
                    let(r,_)=slot.pending.take().unwrap();slot.received_bytes+=r.size_bytes;slot.received.insert(r);}}
            Request::Commit{..}=>{ensure!(slot.pending.is_none()&&slot.partial.is_empty()&&slot.missing.is_empty()&&slot.announced.len()==slot.refs&&slot.announced_bytes==slot.bytes&&slot.received.len()==slot.refs&&slot.received_bytes==slot.bytes&&slot.received.contains(&slot.seed),"paged full transfer incomplete");
                slot.pin_attempted=true;store.publish(slot.stage.as_ref().unwrap(),&slot.import_pin,&slot.seed)?;
                ensure!(store.retained_closure(&slot.seed)?.into_iter().collect::<BTreeSet<_>>()==slot.received,"paged exact closure differs");
                let(descriptor,_,_)=read_paged_seed(&store,&slot.seed,||{slot.budget.remaining()?;Ok(())})?;
                // Establish this exact immutable private floor ONCE per seed.
                // Every task forks its paged namespace without a history walk.
                CowMetadataFloor::prepare_retaining(&mut slot.floor,store.clone(),slot.seed.clone(),&slot.export.root.join("objects"),
                    format!("root-paged-private:{id}"),&slot.budget,slot.cancel.flag.clone())?;
                let manager=VfsManager::open(Path::new(DISK_ROOT).join("managers").join(format!("cow-{id}")),store.clone())?;
                manager.prepare_paged(slot.floor.as_ref().unwrap().root()?)?;slot.cow_manager=Some(manager);slot.descriptor=Some(descriptor);
                let _=directory(&slot.export.root,0o700)?;
                let nonce=uuid::Uuid::new_v4().to_string();slot.export.manager=Some(VfsManager::open(Path::new(DISK_ROOT).join("managers").join(&nonce),store.clone())?);
                let manager=slot.export.manager.as_ref().unwrap();manager.prepare_paged(&slot.seed)?;
                let view=manager.fork_paged(&nonce,&slot.seed,OwnerEpoch{owner:format!("root-seed:{id}"),epoch:1})?;
                RetainedMetadataMount::capture_retaining(&mut slot.export.mount,namespace,view,slot.export.root.clone(),MetadataMountMode::ReadOnlySeed,nonce)?;
                slot.export.mount.as_mut().unwrap().mount_retaining(&slot.budget,&slot.cancel.flag)?;
                slot.cancel.finish()?;slot.cancel.check_success(&slot.budget)?;slot.ready=true;
                return Ok(Reply::PagedReady{slot:id,seed:slot.seed.clone(),root:slot.export.root.clone(),objects:slot.export.root.join("objects"),cow_floor:slot.floor.as_ref().unwrap().root()?.clone()})}
            _=>bail!("unsupported paged transfer request"),
        }slot.budget.remaining()?;Ok(Reply::Accepted)
    }
}
