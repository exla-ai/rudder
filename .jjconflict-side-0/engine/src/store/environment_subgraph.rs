use super::*;
struct State{io:StoreIo,lineage:Option<Arc<EnvironmentPreparationLineage>>,metadata:BTreeMap<ObjectRef,Vec<u8>>,members:BTreeSet<ObjectRef>,
    before:Vec<Option<crate::store::baked_image::BakedVersionSweep>>,after:Vec<Option<crate::store::baked_image::BakedVersionSweep>>,
    binding:Option<EnvironmentBindingData>,attempted:bool,failure:Option<anyhow::Error>}
/// Only actual acknowledged preparation for its EXACT Environment issues this
/// capability. No Clone/Serde/path/receipt constructor, no Root/SDK permission.
pub struct RetainedEnvironmentSubgraph{root:ObjectRef,receipt:ObjectRef,parent:Arc<BackedPinOwner>,state:Mutex<State>,ready:AtomicBool}
impl RetainedEnvironmentSubgraph{
    pub(crate) fn capture_from_preparation_retaining(out:&mut Option<Arc<Self>>,prepared:&RetainedBackedPreparation,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original prepared Environment subgraph occupied");let ack=prepared.acknowledgment()?;
        *out=Some(Arc::new(Self{root:ack.prepared().root.clone(),receipt:ack.prepared().receipt.clone(),parent:ack.retention().clone(),
            state:Mutex::new(State{io:StoreIo::staged(ack.retention().store.clone(),original),lineage:None,metadata:BTreeMap::new(),members:BTreeSet::new(),before:Vec::new(),after:Vec::new(),binding:None,attempted:false,failure:None}),ready:AtomicBool::new(false)}));
        let owner=out.as_ref().unwrap();let mut state=owner.state.lock().unwrap_or_else(|p|p.into_inner());
        ensure!(!state.attempted,"original environment derivation cannot replay");state.attempted=true;
        if let Err(e)=owner.capture_inner(&mut state,prepared){state.failure=Some(e);bail!("original Environment derivation failed; retain lineage/IO/version owners: {:#}",state.failure.as_ref().unwrap())}
        owner.ready.store(true,Ordering::Release);Ok(())
    }
    fn capture_inner(&self,s:&mut State,prepared:&RetainedBackedPreparation)->Result<()> {
        s.io.original.remaining()?;prepared.capture_environment_lineage_retaining(&mut s.lineage,&*s.io.original)?;
        let lineage=s.lineage.as_ref().unwrap();ensure!(lineage.environment()==&self.root&&lineage.receipt()==&self.receipt&&Arc::ptr_eq(lineage.parent(),&self.parent),"original preparation lineage changed");
        ensure!(self.root.kind==ObjectKind::EnvironmentRoot&&self.root.size_bytes<=64*1024&&self.parent.members.contains(&self.root)
            &&self.parent.record.local_objects.binary_search(&self.root).is_ok()&&self.parent.record.root==self.receipt,"exact prepared Environment boundary missing from original receipt");
        for source in &self.parent.sources{s.before.push(None);source.check_original_versions_retaining(s.before.last_mut().unwrap(),s.io.original.clone())?;}
        s.io.acquire(false)?;let retained=s.io.read_state()?;self.parent.check(&s.io,&retained)?;lineage.current(&*s.io.original)?;
        let mut pending=vec![self.root.clone()];let mut work=0u64;let mut total=0u64;
        while let Some(r)=pending.pop(){s.io.current()?;r.validate()?;ensure!(self.parent.members.contains(&r),"Environment edge outside original complete prepared graph");
            if !s.members.insert(r.clone()){continue;}ensure!(s.members.len()<=65_536,"Environment child object count exceeds original profile");
            total=total.checked_add(r.size_bytes).context("Environment child bytes overflow")?;ensure!(total<=2*1024*1024*1024,"Environment child exceeds original2GiB profile");
            if r.kind==ObjectKind::Blob{continue;}
            ensure!((r==self.root||matches!(r.kind,ObjectKind::Directory|ObjectKind::DirectoryShard|ObjectKind::File))&&r.size_bytes<=64*1024,"Environment boundary contains unsupported metadata");
            let raw=parent_bytes(&mut s.io,&retained,&self.parent,&r)?;work=work.checked_add(raw.len()as u64).context("Environment metadata work overflow")?;ensure!(work<=128*1024*1024,"Environment child metadata work bound");
            pending.extend(metadata_references(r.kind,&raw)?);ensure!(pending.len()<=65_536*4,"Environment metadata frontier bound");s.metadata.insert(r,raw);
        }
        snapshot::decode_environment(s.metadata.get(&self.root).context("prepared Environment metadata absent")?)?;
        let reader=Metadata{bytes:&s.metadata,original:&*s.io.original};validate_edges(&reader,&s.metadata,&s.members)?;
        for source in &self.parent.sources{s.after.push(None);source.check_original_versions_retaining(s.after.last_mut().unwrap(),s.io.original.clone())?;}
        self.parent.check(&s.io,&retained)?;lineage.current(&*s.io.original)?;
        s.binding=Some(EnvironmentBindingData{format_version:1,profile:data::PROFILE.into(),boundary:self.root.clone(),preparation_receipt:self.receipt.clone(),
            parent_pin:self.parent.pin.clone(),parent_root:self.parent.record.root.clone(),parent_record_sha256:data::record_digest(&self.parent.record)?,
            closure_sha256:ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&s.members)?).digest,object_count:s.members.len()as u64,encoded_bytes:total,depth:1});
        s.binding.as_ref().unwrap().id_data()?;s.io.original.remaining()?;s.io.release_known()?;Ok(())
    }
    fn locked(&self,original:&dyn ImageImportFence)->Result<MutexGuard<'_,State>>{loop{original.remaining()?;match self.state.try_lock(){Ok(s)=>return Ok(s),Err(TryLockError::WouldBlock)=>std::thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(_)=>bail!("original Environment subgraph poisoned")}}}
    pub fn root_data(&self)->Result<&ObjectRef>{ensure!(self.ready.load(Ordering::Acquire),"original Environment subgraph ACK absent");Ok(&self.root)}
    pub fn parent_retention(&self)->&Arc<BackedPinOwner>{&self.parent}
    pub fn binding_data(&self)->Result<EnvironmentBindingData>{ensure!(self.ready.load(Ordering::Acquire),"original Environment subgraph ACK absent");let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("Environment binding borrowed/poisoned"))?;state.binding.clone().context("original Environment binding absent")}
    pub(super) fn check_locked(&self,io:&StoreIo,state:&RetentionState)->Result<()> {ensure!(self.ready.load(Ordering::Acquire),"original Environment subgraph incomplete");let s=self.locked(&*io.original)?;
        self.parent.check(io,state)?;s.lineage.as_ref().context("original preparation lineage absent")?.current(&*io.original)?;
        let binding=s.binding.as_ref().context("original Environment binding absent")?;ensure!(binding.parent_pin==self.parent.pin&&binding.parent_root==self.parent.record.root
            &&binding.boundary==self.root&&binding.preparation_receipt==self.receipt&&binding.parent_record_sha256==data::record_digest(&self.parent.record)?,"original Environment boundary record differs");io.original.remaining()?;Ok(())}
    pub(super) fn parent(&self)->&Arc<BackedPinOwner>{&self.parent}
    pub(super) fn contains(&self,r:&ObjectRef,original:&dyn ImageImportFence)->Result<bool>{let s=self.locked(original)?;ensure!(self.ready.load(Ordering::Acquire),"Environment subgraph ACK absent");Ok(s.members.contains(r))}
    pub(super) fn environment_bytes(&self,original:&dyn ImageImportFence)->Result<Vec<u8>>{let s=self.locked(original)?;ensure!(self.ready.load(Ordering::Acquire),"Environment subgraph ACK absent");Ok(s.metadata.get(&self.root).context("exact prepared Environment metadata absent")?.clone())}
    pub(super) fn check_versions_retaining(&self,out:&mut Vec<Option<crate::store::baked_image::BakedVersionSweep>>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_empty(),"original environment version observations already attempted");ensure!(self.ready.load(Ordering::Acquire),"Environment subgraph ACK absent");
        for source in &self.parent.sources{out.push(None);source.check_original_versions_retaining(out.last_mut().unwrap(),original.clone())?;}original.remaining()?;Ok(())
    }
}

/// A single original bounded read. Failed and late IO remains in this carrier;
/// an acknowledged immutable preparation is never given a new preparation ACK.
pub struct EnvironmentSubgraphRead{owner:Arc<RetainedEnvironmentSubgraph>,io:StoreIo,reference:ObjectRef,bytes:Option<Vec<u8>>,attempted:bool,ready:bool}
impl RetainedEnvironmentSubgraph{
    pub fn read_retaining(self:&Arc<Self>,out:&mut Option<EnvironmentSubgraphRead>,reference:ObjectRef,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original Environment read occupied");*out=Some(EnvironmentSubgraphRead{owner:self.clone(),io:StoreIo::staged(self.parent.store.clone(),original),reference,bytes:None,attempted:false,ready:false});out.as_mut().unwrap().run()
    }
}
impl EnvironmentSubgraphRead{
    fn run(&mut self)->Result<()>{ensure!(!self.attempted,"original Environment read cannot replay");self.attempted=true;self.io.acquire(false)?;let state=self.io.read_state()?;
        self.owner.check_locked(&self.io,&state)?;ensure!(self.owner.contains(&self.reference,&*self.io.original)?,"read outside exact original Environment subgraph");
        self.bytes=Some(parent_bytes(&mut self.io,&state,self.owner.parent(),&self.reference)?);self.owner.check_locked(&self.io,&state)?;
        self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())}
    pub fn verified_bytes(&self)->Result<&[u8]>{ensure!(self.ready,"original Environment read incomplete/unknown");self.bytes.as_deref().context("original Environment bytes absent")}
}
