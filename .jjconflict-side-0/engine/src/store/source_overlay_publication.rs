use super::*;
use crate::content::{decode_file_node,validate_file_child};
use crate::directory::validate_child;
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub struct SourceOverlayWorkData{pub verified_delta_objects:u64,pub verified_delta_bytes:u64,pub reused_parent_edges:u64,pub source_entries:u64,pub source_logical_bytes:u64,
    /// Actual successful local object hash/read and object fsync calls in this
    /// original publication only. Excludes retention files, directory syncs,
    /// foreign reads and earlier parent work. Totals are available only after
    /// the original publication completes; failed/unknown carriers keep zero.
    pub actual_io_complete:bool,pub actual_local_hash_reads:u64,pub actual_local_hash_bytes:u64,
    pub actual_local_object_syncs:u64,pub actual_local_object_sync_bytes:u64}
struct Delta{members:BTreeSet<ObjectRef>,bytes:u64,shape:Shape}
fn digest(members:&BTreeSet<ObjectRef>)->Result<String>{Ok(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(members)?).digest)}
fn graph(io:&mut StoreIo,state:&RetentionState,root:&ObjectRef,parent:&RetainedSourceParent,work:&mut SourceOverlayWorkData)->Result<Delta>{
    parent.check(io,state)?;let binding=parent.binding_data()?;ensure!(root.kind==ObjectKind::RunnableRoot,"source overlay needs RunnableRoot");
    let environment=parent.owner.environment();let env=environment.root_data()?;let mut members=BTreeSet::new();let mut new=BTreeMap::new();let mut pending=vec![root.clone()];let mut bytes=0u64;let mut metadata_bytes=0u64;
    while let Some(r)=pending.pop(){io.current()?;
        if &r==env{continue;}if parent.members.contains(&r){work.reused_parent_edges=work.reused_parent_edges.checked_add(1).context("source reuse counter overflow")?;continue;}
        if !members.insert(r.clone()){continue;}
        ensure!(binding.local_objects.checked_add(members.len()as u64).is_some_and(|n|n<=data::MAX_LOCAL_OBJECTS),"source parent+delta object bound; explicit compaction required");
        bytes=bytes.checked_add(r.size_bytes).context("source delta bytes overflow")?;ensure!(binding.local_bytes.checked_add(bytes).is_some_and(|n|n<=data::MAX_LOCAL_BYTES),"source parent+delta byte bound; explicit compaction required");
        let raw=io.read_required_object(&r,bound(&r,&[])?)?;
        work.verified_delta_objects+=1;work.verified_delta_bytes=work.verified_delta_bytes.checked_add(raw.len()as u64).context("source verified-byte counter overflow")?;
        if r.kind!=ObjectKind::Blob{metadata_bytes=metadata_bytes.checked_add(raw.len()as u64).context("source metadata bytes overflow")?;ensure!(metadata_bytes<=128*1024*1024,"source new metadata bound");
            pending.extend(metadata_references(r.kind,&raw)?);ensure!(pending.len()<=100_000*4,"source overlay frontier bound");new.insert(r,raw);}
    }
    let runnable=snapshot::decode_runnable(metadata(&new,Some(parent),root)?)?;
    let source=snapshot::decode_source(metadata(&new,Some(parent),&runnable.source)?)?;
    ensure!(runnable.environment.as_ref()==Some(env)&&runnable.view_root==source.directory
        &&runnable.consistency==CheckpointConsistency::Quiesced&&source.consistency==CheckpointConsistency::Quiesced
        &&source.projection_policy==binding.projection_policy,"source overlay changed exact Environment/policy or quiesced source binding");
    for(r,raw)in &new{io.original.remaining()?;
        for child in metadata_references(r.kind,raw)?{
            if &child==env{ensure!(r.kind==ObjectKind::RunnableRoot,"Environment boundary outside RunnableRoot edge");snapshot::validate_child(raw,&child,&environment.environment_bytes(&*io.original)?)?;continue;}
            ensure!(members.contains(&child)||parent.members.contains(&child),"source typed child outside original parent/new delta");if child.kind==ObjectKind::Blob{continue;}
            let raw_child=metadata(&new,Some(parent),&child)?;
            match r.kind{ObjectKind::Directory|ObjectKind::DirectoryShard=>validate_child(r.kind,raw,&child,raw_child)?,
                ObjectKind::File=>{let node=decode_file_node(raw)?;if child.kind==ObjectKind::File{let c=decode_file_node(raw_child)?;for(i,r)in node.children.iter().enumerate(){if r==&child{validate_file_child(&node,i,&c)?;}}}},
                ObjectKind::RunnableRoot if child.kind==ObjectKind::SourceRoot=>snapshot::validate_child(raw,&child,raw_child)?,_=>{}}
        }
    }
    let shapes=shape(&source.directory,&new,Some(parent),&*io.original)?;let shape=shapes[&source.directory];work.source_entries=shape.entries;work.source_logical_bytes=shape.logical;
    parent.check(io,state)?;io.current()?;Ok(Delta{members,bytes,shape})
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]pub enum SourceOverlayOrigin{OriginalPublication,ExplicitReattachment}
pub struct SourceOverlayPinOwner{store:LocalStore,custody:Arc<StoreCustody>,parent:Arc<RetainedSourceParent>,pin:String,record:SourceOverlayPinData,delta:Arc<BTreeSet<ObjectRef>>,failed:AtomicBool,origin:SourceOverlayOrigin}
impl SourceOverlayPinOwner{
    pub fn root_data(&self)->&ObjectRef{&self.record.root}
    pub fn record_data(&self)->&SourceOverlayPinData{&self.record}
    pub fn source_parent(&self)->&Arc<RetainedSourceParent>{&self.parent}
    pub fn origin_data(&self)->SourceOverlayOrigin{self.origin}
    pub fn pin_name_data(&self)->&str{&self.pin}
    pub fn store_root_data(&self)->&std::path::Path{self.store.root()}
    /// Membership is DATA; actual use still needs this owner's verified read
    /// and the caller's current project/source/execution gates.
    pub fn contains_data(&self,r:&ObjectRef,original:&dyn ImageImportFence)->Result<bool>{
        original.remaining()?;Ok(self.delta.contains(r)||self.parent.owner.contains_data(r,original)?)
    }
    pub fn matches_record_data(&self,state:&RetentionState,pin:&str)->bool{
        self.pin==pin&&state.source_dependencies.as_ref().is_some_and(|data|
            data.pins.get(pin)==Some(&self.record)&&self.parent.binding_data().is_ok_and(|binding|data.parents.get(&self.record.parent_id)==Some(binding)))
    }
    fn check(&self,io:&StoreIo,state:&RetentionState)->Result<()> {
        ensure!(!self.failed.load(Ordering::Acquire),"original source overlay physically fenced");ensure!(io.store.root()==self.store.root(),"source overlay store differs");
        let custody=self.custody.current(io);if custody.as_ref().err().is_some_and(observed_failure){self.failed.store(true,Ordering::Release);self.parent.owner.fence_local_source();}custody?;self.parent.check(io,state)?;
        let data=state.source_dependencies.as_ref().context("original source overlay record absent")?;
        let same=data.pins.get(&self.pin)==Some(&self.record)&&self.parent.binding_data()?.id_data()?==self.record.parent_id
            &&data.parents.get(&self.record.parent_id)==Some(self.parent.binding_data()?);
        if !same{self.failed.store(true,Ordering::Release);bail!("original source overlay pin/parent changed")};io.original.remaining()?;Ok(())
    }
}
pub struct SourceOverlayPublication{io:StoreIo,parent:Arc<RetainedSourceParent>,stage:String,pin:String,root:ObjectRef,operation:String,attempted:bool,ready:bool,
    work:SourceOverlayWorkData,ack:Option<Arc<SourceOverlayPinOwner>>,failure:Option<anyhow::Error>,#[cfg(test)]fault:Option<OverlayTestPoint>}
impl SourceOverlayPublication{
    fn staged(store:LocalStore,parent:Arc<RetainedSourceParent>,stage:String,pin:String,root:ObjectRef,original:Arc<dyn ImageImportFence>)->Self{
        Self{io:StoreIo::staged(store,original),parent,stage,pin,root,operation:Uuid::new_v4().to_string(),attempted:false,ready:false,work:SourceOverlayWorkData::default(),ack:None,failure:None,#[cfg(test)]fault:None}}
    pub fn acknowledgment(&self)->Result<&Arc<SourceOverlayPinOwner>>{ensure!(self.ready,"original source overlay ACK incomplete/unknown");self.ack.as_ref().context("actual source overlay owner absent")}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    pub fn work_data(&self)->SourceOverlayWorkData{self.work}
    fn run(&mut self)->Result<()>{ensure!(!self.attempted,"original source overlay publication cannot replay");self.attempted=true;
        if let Err(e)=self.run_inner(){self.failure=Some(e);bail!("source overlay publication failed; retain original IO/intent/parents")};Ok(())}
    fn run_inner(&mut self)->Result<()> {
        validate_pin(&self.pin)?;let mut state=source_state(&mut self.io,&self.parent.owner,true)?;self.parent.check(&self.io,&state)?;let binding=self.parent.binding_data()?.clone();let parent_id=binding.id_data()?;
        ensure!(state.staging.get(&self.stage).is_some_and(|s|s.expires_at_ms>now_ms().unwrap_or(u64::MAX)),"source overlay original staging absent/expired");
        ensure!(!state.pins.contains_key(&self.pin)&&!state.backed_retention.as_ref().is_some_and(|d|d.pins.contains_key(&self.pin))
            &&!state.environment_dependencies.as_ref().is_some_and(|d|d.pins.contains_key(&self.pin)),"source overlay pin conflicts with existing root");
        let source_pins=state.source_dependencies.as_ref().map_or(0,|d|d.pins.len());
        let source_holds=state.source_dependencies.as_ref().map_or(0,|d|d.holds.len());
        let base=state.environment_dependencies.as_ref().context("actual flat source namespace absent")?;
        ensure!(base.pins.len()+source_pins<data::MAX_DEPENDENT_PINS&&base.holds.len()+source_holds<data::MAX_DEPENDENT_HOLDS,"combined source dependency capacity reached");
        ensure!(!base.holds.values().any(|h|h.pin==self.pin)&&!state.backed_retention.as_ref().is_some_and(|d|d.holds.values().any(|h|h.pin==self.pin)),"source overlay conflicts with another original hold");
        let data=state.source_dependencies.get_or_insert_with(SourceDependenciesData::default);
        ensure!(!data.pins.contains_key(&self.pin)&&!data.holds.values().any(|h|h.pin==self.pin),"source original publication exists/unknown; no retry");
        if let Some(saved)=data.parents.get(&parent_id){ensure!(saved==&binding,"source parent binding collision");}else{data.parents.insert(parent_id.clone(),binding);}
        data.holds.insert(self.operation.clone(),SourceOverlayHoldData{format_version:1,operation_id:self.operation.clone(),pin:self.pin.clone(),root:self.root.clone(),parent_id:parent_id.clone(),staging_id:self.stage.clone()});
        state=self.io.write_state(state)?;#[cfg(test)]if self.fault==Some(OverlayTestPoint::AfterHold){bail!("injected source hold outcome unknown")}
        self.io.durable_reads=true;let delta=graph(&mut self.io,&state,&self.root,&self.parent,&mut self.work)?;
        let shards=self.io.shards.values().map(|(f,_)|f.try_clone()).collect::<std::io::Result<Vec<_>>>()?;for shard in shards{self.io.sync_directory(shard)?;}
        self.io.sync_directory(self.io.objects.as_ref().unwrap().try_clone()?)?;self.io.sync_directory(self.io.tmp.as_ref().unwrap().try_clone()?)?;
        self.parent.check(&self.io,&state)?;ensure!(state.staging[&self.stage].expires_at_ms>now_ms()?,"source overlay stage expired before publication");
        let record=SourceOverlayPinData{format_version:1,operation_id:self.operation.clone(),root:self.root.clone(),parent_id,delta_sha256:digest(&delta.members)?,delta_objects:delta.members.len()as u64,delta_bytes:delta.bytes,
            source_entries:delta.shape.entries,source_logical_bytes:delta.shape.logical,source_height:delta.shape.height,depth:3,generation:state.generation.checked_add(1).context("source overlay generation overflow")?};
        let data=state.source_dependencies.as_mut().unwrap();data.pins.insert(self.pin.clone(),record.clone());data.holds.remove(&self.operation);state.staging.remove(&self.stage);
        #[cfg(test)]if self.fault==Some(OverlayTestPoint::FinalRename){self.io.fail_after_rename=true;}
        state=self.io.write_state(state)?;#[cfg(test)]if self.fault==Some(OverlayTestPoint::AfterCommit){bail!("injected source late ACK")}
        self.ack=Some(Arc::new(SourceOverlayPinOwner{store:self.io.store.clone(),custody:StoreCustody::capture(&self.io)?,parent:self.parent.clone(),pin:self.pin.clone(),record,delta:Arc::new(delta.members),failed:AtomicBool::new(false),origin:SourceOverlayOrigin::OriginalPublication}));
        self.ack.as_ref().unwrap().check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;
        self.work.actual_local_hash_reads=self.io.actual_local_hash_reads;self.work.actual_local_hash_bytes=self.io.actual_local_hash_bytes;
        self.work.actual_local_object_syncs=self.io.actual_local_object_syncs;self.work.actual_local_object_sync_bytes=self.io.actual_local_object_sync_bytes;
        self.work.actual_io_complete=true;self.ready=true;Ok(())
    }
    #[cfg(test)]pub(crate) fn io_reads_data(&self)->&BTreeMap<ObjectRef,u64>{&self.io.object_read_bytes}
    #[cfg(test)]pub(crate) fn io_syncs_data(&self)->&BTreeMap<ObjectRef,u64>{&self.io.object_syncs}
}
impl LocalStore{
    pub fn publish_source_overlay_retaining(&self,out:&mut Option<SourceOverlayPublication>,stage:String,pin:String,root:ObjectRef,parent:Arc<RetainedSourceParent>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original source overlay output occupied");*out=Some(SourceOverlayPublication::staged(self.clone(),parent,stage,pin,root,original));out.as_mut().unwrap().run()
    }
    #[cfg(test)]pub(crate) fn publish_source_overlay_fault_retaining(&self,out:&mut Option<SourceOverlayPublication>,stage:String,pin:String,root:ObjectRef,parent:Arc<RetainedSourceParent>,original:Arc<dyn ImageImportFence>,fault:OverlayTestPoint)->Result<()> {
        ensure!(out.is_none(),"source overlay test output occupied");*out=Some(SourceOverlayPublication::staged(self.clone(),parent,stage,pin,root,original));let held=out.as_mut().unwrap();held.fault=Some(fault);held.run()
    }
}

pub struct SourceOverlayRead{owner:Arc<SourceOverlayPinOwner>,io:StoreIo,reference:ObjectRef,bytes:Option<Vec<u8>>,attempted:bool,ready:bool}
impl SourceOverlayPinOwner{
    pub fn read_retaining(self:&Arc<Self>,out:&mut Option<SourceOverlayRead>,reference:ObjectRef,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original source overlay read occupied");*out=Some(SourceOverlayRead{owner:self.clone(),io:StoreIo::staged(self.store.clone(),original),reference,bytes:None,attempted:false,ready:false});out.as_mut().unwrap().run()
    }
}
impl SourceOverlayPinOwner{
    #[cfg(test)]pub(crate) fn read_initial_open_error_retaining(self:&Arc<Self>,out:&mut Option<SourceOverlayRead>,reference:ObjectRef,original:Arc<dyn ImageImportFence>,errno:i32)->Result<()>{
        ensure!(out.is_none(),"source overlay test read occupied");*out=Some(SourceOverlayRead{owner:self.clone(),io:StoreIo::staged(self.store.clone(),original),reference,bytes:None,attempted:false,ready:false});
        let read=out.as_mut().unwrap();read.io.capture_open_error=Some(errno);read.run()
    }
}
impl SourceOverlayRead{
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"source overlay read cannot replay");self.attempted=true;let state=source_state(&mut self.io,&self.owner.parent.owner,false)?;self.owner.check(&self.io,&state)?;
        self.bytes=Some(if self.owner.delta.contains(&self.reference)||self.owner.parent.members.contains(&self.reference){
            let inherited=self.owner.parent.members.contains(&self.reference);let result=self.io.read_required_object(&self.reference,bound(&self.reference,&[])?);
            if result.as_ref().err().is_some_and(observed_failure){
                if inherited{self.owner.parent.owner.fence_local_source();}else{self.owner.failed.store(true,Ordering::Release);}
            }
            result?}
            else{let environment=self.owner.parent.owner.environment();ensure!(environment.contains(&self.reference,&*self.io.original)?,"read outside original source/Environment dependency graph");parent_bytes(&mut self.io,&state,environment.parent(),&self.reference)?});
        self.owner.check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())
    }
    pub fn verified_bytes(&self)->Result<&[u8]>{ensure!(self.ready,"source overlay read incomplete/unknown");self.bytes.as_deref().context("source overlay actual bytes absent")}
}
pub struct SourceOverlayReattachment{io:StoreIo,parent:Arc<RetainedSourceParent>,pin:String,owner:Option<Arc<SourceOverlayPinOwner>>,attempted:bool,ready:bool,work:SourceOverlayWorkData,failure:Option<anyhow::Error>}
impl LocalStore{
    pub fn reattach_source_overlay_retaining(&self,out:&mut Option<SourceOverlayReattachment>,pin:String,parent:Arc<RetainedSourceParent>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"source reattachment output occupied");*out=Some(SourceOverlayReattachment{io:StoreIo::staged(self.clone(),original),parent,pin,owner:None,attempted:false,ready:false,work:SourceOverlayWorkData::default(),failure:None});let held=out.as_mut().unwrap();
        if let Err(e)=held.run(){held.failure=Some(e);bail!("source overlay reattachment failed; retain new original IO")};Ok(())
    }
}
impl SourceOverlayReattachment{
    pub fn reattached_owner(&self)->Result<&Arc<SourceOverlayPinOwner>>{ensure!(self.ready,"source reattachment incomplete/unknown");self.owner.as_ref().context("source reattached owner absent")}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"source reattachment cannot replay");self.attempted=true;validate_pin(&self.pin)?;let state=source_state(&mut self.io,&self.parent.owner,true)?;
        self.parent.check(&self.io,&state)?;
        ensure!(self.parent.owner.origin_data()==DependentOwnerOrigin::ExplicitReattachment,"source reattachment needs an explicitly fully revalidated flat source parent");
        let data=state.source_dependencies.as_ref().context("source record DATA absent")?;ensure!(data.holds.is_empty(),"unknown original source publication still held");let record=data.pins.get(&self.pin).context("source pin DATA absent")?.clone();
        ensure!(self.parent.binding_data()?.id_data()?==record.parent_id&&data.parents.get(&record.parent_id)==Some(self.parent.binding_data()?),"source original parent differs");
        self.io.durable_reads=true;let delta=graph(&mut self.io,&state,&record.root,&self.parent,&mut self.work)?;check_delta(&record,&delta)?;
        let shards=self.io.shards.values().map(|(f,_)|f.try_clone()).collect::<std::io::Result<Vec<_>>>()?;for shard in shards{self.io.sync_directory(shard)?;}
        self.io.sync_directory(self.io.objects.as_ref().unwrap().try_clone()?)?;self.io.sync_directory(self.io.tmp.as_ref().unwrap().try_clone()?)?;
        self.owner=Some(Arc::new(SourceOverlayPinOwner{store:self.io.store.clone(),custody:StoreCustody::capture(&self.io)?,parent:self.parent.clone(),pin:self.pin.clone(),record,delta:Arc::new(delta.members),failed:AtomicBool::new(false),origin:SourceOverlayOrigin::ExplicitReattachment}));
        self.owner.as_ref().unwrap().check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())
    }
}
fn check_delta(record:&SourceOverlayPinData,delta:&Delta)->Result<()> {
    ensure!(record.delta_sha256==digest(&delta.members)?&&record.delta_objects==delta.members.len()as u64&&record.delta_bytes==delta.bytes
        &&record.source_entries==delta.shape.entries&&record.source_logical_bytes==delta.shape.logical&&record.source_height==delta.shape.height,"source original delta graph/shape differs");Ok(())
}
pub(in crate::store::backed_retention) fn gc_check(owners:&[Arc<SourceOverlayPinOwner>],io:&StoreIo,state:&RetentionState)->Result<()> {
    let expected=state.source_dependencies.as_ref().map_or(0,|d|d.pins.len());ensure!(owners.len()==expected,"actual source overlay owners absent/extra; no GC");
    let mut names=BTreeSet::new();for owner in owners{ensure!(names.insert(owner.pin.clone()),"duplicate source overlay GC owner");owner.check(io,state)?;}
    if let Some(data)=&state.source_dependencies{ensure!(data.holds.is_empty(),"unknown source publication blocks GC");}Ok(())
}
pub(in crate::store::backed_retention) fn gc_local(owners:&[Arc<SourceOverlayPinOwner>],io:&mut StoreIo,state:&RetentionState)->Result<BTreeSet<String>> {
    let mut result=BTreeSet::new();for owner in owners{owner.check(io,state)?;
        let scanned=graph(io,state,&owner.record.root,&owner.parent,&mut SourceOverlayWorkData::default());
        // A physically failed private delta cannot become usable again after
        // an external repair. Cancellation alone is still an unknown effect.
        if scanned.as_ref().err().is_some_and(observed_failure){owner.failed.store(true,Ordering::Release);}
        let delta=scanned?;check_delta(&owner.record,&delta)?;result.extend(delta.members.into_iter().map(|r|r.digest));}Ok(result)
}
#[cfg(test)]#[derive(Clone,Copy,Debug,PartialEq,Eq)]pub(crate) enum OverlayTestPoint{AfterHold,FinalRename,AfterCommit}
