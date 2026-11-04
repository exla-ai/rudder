//! Sibling-only inheritance of a genuinely published immutable source.
//! DATA/retention owners, never source acceptance, project or execution grants.
use super::*;
use crate::store::source_dependency_data::{self as source_data,SourceParentBindingData,SourceOverlayPinData,SourceOverlayHoldData,SourceDependenciesData};
use crate::directory::{decode_directory_root,decode_shard,ShardNode};
use crate::protocol::CheckpointConsistency;
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
struct Shape{entries:u64,logical:u64,height:u64}
fn add(a:&mut Shape,b:Shape)->Result<()>{a.entries=a.entries.checked_add(b.entries).context("source summary entries overflow")?;
    a.logical=a.logical.checked_add(b.logical).context("source summary bytes overflow")?;a.height=a.height.max(b.height);Ok(())}
fn metadata<'a>(new:&'a BTreeMap<ObjectRef,Vec<u8>>,parent:Option<&'a RetainedSourceParent>,r:&ObjectRef)->Result<&'a [u8]>{
    new.get(r).or_else(||parent.and_then(|p|p.metadata.get(r))).map(Vec::as_slice).context("typed source metadata absent from actual new/parent owners")}
// Iterative postorder avoids recursive worker-stack growth. Cached summaries
// cover original radix shards as well as directories, so a flat100k-entry
// directory does not cause a full scan when only one radix leaf changes.
fn shape(root:&ObjectRef,new:&BTreeMap<ObjectRef,Vec<u8>>,parent:Option<&RetainedSourceParent>,original:&dyn ImageImportFence)->Result<BTreeMap<ObjectRef,Shape>>{
    let mut done=BTreeMap::new();let mut visiting=BTreeSet::new();let mut pending=vec![(root.clone(),false)];
    while let Some((r,fold))=pending.pop(){original.remaining()?;if done.contains_key(&r){continue;}
        if let Some(value)=parent.and_then(|p|p.shapes.get(&r)){done.insert(r,*value);continue;}
        let raw=metadata(new,parent,&r)?;
        if !fold{
            ensure!(visiting.insert(r.clone()),"source metadata summary cycle");pending.push((r.clone(),true));
            let children=match r.kind{
                ObjectKind::Directory=>vec![decode_directory_root(raw)?.directory_shard],
                ObjectKind::DirectoryShard=>match decode_shard(raw)?.node{
                    ShardNode::Branch{children}=>children.into_iter().map(|c|c.shard.reference()).collect(),
                    ShardNode::Leaf{entries}=>entries.into_iter().filter_map(|e|match e.node{crate::directory::EntryNode::Directory{object}=>Some(object),_=>None}).collect()},
                _=>bail!("source summary requires paged Directory/DirectoryShard")};
            for child in children{if !done.contains_key(&child){pending.push((child,false));}}
            ensure!(pending.len()<=100_000*4&&visiting.len()<=100_000,"source summary work bound");continue;
        }
        let mut value=Shape::default();match r.kind{
            ObjectKind::Directory=>value=*done.get(&decode_directory_root(raw)?.directory_shard).context("source radix summary child absent")?,
            ObjectKind::DirectoryShard=>match decode_shard(raw)?.node{
                ShardNode::Branch{children}=>for child in children{add(&mut value,*done.get(&child.shard.reference()).context("source branch summary absent")?)?;},
                ShardNode::Leaf{entries}=>for entry in entries{value.entries=value.entries.checked_add(1).context("source leaf count overflow")?;
                    match entry.node{crate::directory::EntryNode::Directory{object}=>{let mut child=*done.get(&object).context("source nested summary absent")?;child.height=child.height.checked_add(1).context("source height overflow")?;add(&mut value,child)?;},
                        crate::directory::EntryNode::File{logical_size,..}=>value.logical=value.logical.checked_add(logical_size).context("source logical bytes overflow")?,_=>{}}}},
            _=>unreachable!()}
        ensure!(value.entries<=data::MAX_LOCAL_OBJECTS&&value.logical<=data::MAX_LOCAL_BYTES&&value.height<=crate::directory::MAX_PATH_DEPTH as u64,"source logical/path/depth profile exceeded");
        visiting.remove(&r);done.insert(r,value);
    }Ok(done)
}
// Initial custody/retention observations belong to the exact source anchor.
// Keep arbitrary IO errors and clock cancellation unknown; only tagged actual
// physical observations can absorb into the original shared parent owner.
pub(super) fn source_state(io:&mut StoreIo,owner:&Arc<EnvironmentDependentPinOwner>,exclusive:bool)->Result<RetentionState>{
    ensure!(io.store.root()==owner.store_root_data(),"source initial IO belongs to another actual store");
    let result=(||->Result<RetentionState>{io.acquire(exclusive)?;io.read_state()})();
    if result.as_ref().err().is_some_and(observed_failure){owner.fence_local_source();}result
}
pub(in crate::store::backed_retention) fn fence_initial_io(dependents:&[Arc<EnvironmentDependentPinOwner>],overlays:&[Arc<SourceOverlayPinOwner>],root:&std::path::Path){
    for owner in dependents{if owner.store_root_data()==root{owner.fence_local_source();}}
    for owner in overlays{if owner.store_root_data()==root{owner.source_parent().original_owner().fence_local_source();}}
}
pub struct RetainedSourceParent{owner:Arc<EnvironmentDependentPinOwner>,io:StoreIo,members:Arc<BTreeSet<ObjectRef>>,metadata:BTreeMap<ObjectRef,Vec<u8>>,
    shapes:BTreeMap<ObjectRef,Shape>,binding:Option<SourceParentBindingData>,ready:bool,failure:Option<anyhow::Error>}
impl EnvironmentDependentPinOwner{
    pub fn derive_source_parent_retaining(self:&Arc<Self>,out:&mut Option<Arc<RetainedSourceParent>>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original source parent derivation occupied");
        *out=Some(Arc::new(RetainedSourceParent{owner:self.clone(),io:StoreIo::staged(self.local_store().clone(),original),
            members:self.local_members().clone(),metadata:BTreeMap::new(),shapes:BTreeMap::new(),binding:None,ready:false,failure:None}));
        let held=Arc::get_mut(out.as_mut().unwrap()).context("source parent aliased before capture")?;
        if let Err(error)=held.capture(){if observed_failure(&error){held.owner.fence_local_source();}held.failure=Some(error);bail!("original source parent derivation failed; retain original IO")};Ok(())
    }
}
impl RetainedSourceParent{
    fn capture(&mut self)->Result<()> {
        let state=source_state(&mut self.io,&self.owner,false)?;self.owner.check(&self.io,&state)?;let mut bytes=0u64;
        for r in self.members.iter(){self.io.original.remaining()?;
            if r.kind!=ObjectKind::Blob{bytes=bytes.checked_add(r.size_bytes).context("source metadata work overflow")?;ensure!(bytes<=128*1024*1024,"source metadata capture bound");
                let raw=self.io.read_required_object(r,bound(r,&[])?)?;self.metadata.insert(r.clone(),raw);}}
        let runnable=snapshot::decode_runnable(self.metadata.get(self.owner.root_data()).context("original parent Runnable metadata absent")?)?;
        let source=snapshot::decode_source(self.metadata.get(&runnable.source).context("original parent Source metadata absent")?)?;
        ensure!(runnable.environment.as_ref()==Some(self.owner.environment().root_data()?)&&runnable.view_root==source.directory
            &&runnable.consistency==CheckpointConsistency::Quiesced&&source.consistency==CheckpointConsistency::Quiesced,"source parent immutable baseline differs");
        self.shapes=shape(&source.directory,&self.metadata,None,&*self.io.original)?;let summary=self.shapes[&source.directory];let record=self.owner.record_data();
        self.binding=Some(SourceParentBindingData{format_version:1,profile:source_data::SOURCE_OVERLAY_PROFILE.into(),parent_pin:self.owner.pin_name_data().into(),
            parent_record_sha256:ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(record)?).digest,runnable:self.owner.root_data().clone(),source:runnable.source,directory:source.directory,
            hardlinks:runnable.hardlinks,projection_policy:source.projection_policy,environment_id:record.environment_id.clone(),local_graph_sha256:record.local_graph_sha256.clone(),
            local_objects:record.local_objects,local_bytes:record.local_bytes,source_entries:summary.entries,source_logical_bytes:summary.logical,source_height:summary.height,depth:2});
        self.binding.as_ref().unwrap().id_data()?;self.owner.check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())
    }
    pub fn binding_data(&self)->Result<&SourceParentBindingData>{ensure!(self.ready,"original source parent derivation incomplete/unknown");self.binding.as_ref().context("source parent binding absent")}
    pub fn original_owner(&self)->&Arc<EnvironmentDependentPinOwner>{&self.owner}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    fn check(&self,io:&StoreIo,state:&RetentionState)->Result<()> {
        let binding=self.binding_data()?;ensure!(io.store.root()==self.owner.store_root_data(),"source parent belongs to another actual store");let current=self.owner.check(io,state);
        let changed=state.environment_dependencies.as_ref().and_then(|d|d.pins.get(self.owner.pin_name_data()))!=Some(self.owner.record_data());
        if changed||current.as_ref().err().is_some_and(observed_failure){self.owner.fence_local_source();}
        current?;
        ensure!(binding.parent_record_sha256==ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(self.owner.record_data())?).digest,"original source parent record changed");
        if let Some(data)=&state.source_dependencies{if let Some(saved)=data.parents.get(&binding.id_data()?){ensure!(saved==binding,"source parent persistent binding changed");}}
        io.original.remaining()?;Ok(())
    }
}

#[path="source_overlay_publication.rs"]mod publication;
pub use publication::{SourceOverlayPublication,SourceOverlayPinOwner,SourceOverlayRead,SourceOverlayReattachment,SourceOverlayWorkData,SourceOverlayOrigin};
pub(in crate::store::backed_retention) use publication::{gc_check,gc_local};
#[cfg(test)]pub(crate) use publication::OverlayTestPoint;
impl LocalStore{
    pub fn gc_source_overlays_retaining(&self,out:&mut Option<BackedGc>,parents:Vec<Arc<BackedPinOwner>>,sources:Vec<Arc<EnvironmentDependentPinOwner>>,
        overlays:Vec<Arc<SourceOverlayPinOwner>>,grace:Duration,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"source-aware GC output occupied");*out=Some(BackedGc{io:StoreIo::staged(self.clone(),original),owners:parents,dependency_owners:sources,source_owners:overlays,
            grace,report:None,object_sets:None,candidates:Vec::new(),unlink_intent:None,attempted:false,ready:false,failure:None});out.as_mut().unwrap().run()
    }
}
