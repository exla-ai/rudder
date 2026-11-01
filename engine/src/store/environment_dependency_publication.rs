use super::*;
use crate::directory::{EntryNode as PagedNode,validate_child};
use crate::content::{decode_file_node,validate_file_child};
struct LocalGraph{members:BTreeSet<ObjectRef>,metadata:BTreeMap<ObjectRef,Vec<u8>>,bytes:u64,entries:u64,logical_bytes:u64}
fn local_digest(graph:&LocalGraph)->Result<String>{Ok(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&graph.members)?).digest)}
fn local_graph(io:&mut StoreIo,state:&RetentionState,root:&ObjectRef,environment:&Arc<RetainedEnvironmentSubgraph>)->Result<LocalGraph>{
    environment.check_locked(io,state)?;ensure!(root.kind==ObjectKind::RunnableRoot,"dependent profile needs a RunnableRoot");
    let boundary=environment.root_data()?.clone();let env_bytes=environment.environment_bytes(&*io.original)?;
    let mut graph=LocalGraph{members:BTreeSet::new(),metadata:BTreeMap::new(),bytes:0,entries:0,logical_bytes:0};let mut pending=vec![root.clone()];let mut boundary_seen=false;
    while let Some(r)=pending.pop(){io.current()?;if r==boundary{boundary_seen=true;continue;}if !graph.members.insert(r.clone()){continue;}
        ensure!(graph.members.len()as u64<=data::MAX_LOCAL_OBJECTS,"dependent local object count bound");
        let maximum=bound(&r,&[])?;graph.bytes=graph.bytes.checked_add(r.size_bytes).context("dependent local bytes overflow")?;ensure!(graph.bytes<=data::MAX_LOCAL_BYTES,"dependent local source bytes bound");
        let raw=io.read_object(&r,maximum)?.context("dependent local graph object absent; no backing fallback outside exact Environment boundary")?;
        if r.kind!=ObjectKind::Blob{pending.extend(metadata_references(r.kind,&raw)?);ensure!(pending.len()<=100_000*4,"dependent local frontier bound");graph.metadata.insert(r,raw);}
    }
    ensure!(boundary_seen,"dependent RunnableRoot has no exact acknowledged Environment edge");
    let runnable=snapshot::decode_runnable(graph.metadata.get(root).context("dependent root metadata absent")?)?;
    ensure!(runnable.environment.as_ref()==Some(&boundary),"dependent root changed actual prepared Environment");
    let source=snapshot::decode_source(graph.metadata.get(&runnable.source).context("dependent source metadata absent")?)?;
    ensure!(runnable.view_root==source.directory&&runnable.consistency==CheckpointConsistency::Quiesced&&source.consistency==CheckpointConsistency::Quiesced,"dependent first profile must contain a quiesced complete source-only view");
    let reader=Metadata{bytes:&graph.metadata,original:&*io.original};
    for(r,raw)in &graph.metadata{io.original.remaining()?;
        if r.kind==ObjectKind::Directory&&serde_json::from_slice::<serde_json::Value>(raw)?.get("format_version").and_then(serde_json::Value::as_u64)==Some(2){PagedDirectory::new(&reader).validate_directory(r,100_000)?;}
        for child in metadata_references(r.kind,raw)?{
            if child==boundary{ensure!(r.kind==ObjectKind::RunnableRoot,"Environment boundary outside typed RunnableRoot edge");snapshot::validate_child(raw,&child,&env_bytes)?;continue;}
            ensure!(graph.members.contains(&child),"dependent typed local child absent");if child.kind==ObjectKind::Blob{continue;}
            let child_bytes=graph.metadata.get(&child).context("dependent child metadata absent")?;
            match r.kind{ObjectKind::Directory|ObjectKind::DirectoryShard=>{let paged=r.kind==ObjectKind::DirectoryShard||serde_json::from_slice::<serde_json::Value>(raw)?.get("format_version").and_then(serde_json::Value::as_u64)==Some(2);
                if paged&&matches!(child.kind,ObjectKind::DirectoryShard|ObjectKind::File){validate_child(r.kind,raw,&child,child_bytes)?;}},
                ObjectKind::File=>{let parent=decode_file_node(raw)?;if child.kind==ObjectKind::File{let node=decode_file_node(child_bytes)?;for(i,c)in parent.children.iter().enumerate(){if c==&child{validate_file_child(&parent,i,&node)?;}}}},
                ObjectKind::RunnableRoot if child.kind==ObjectKind::SourceRoot=>snapshot::validate_child(raw,&child,child_bytes)?,_=>{}}
        }
    }
    // Count actual SOURCE paths/logical bytes as well as unique CAS bytes.
    // Repeated identical files/directories never evade the declared profile.
    let mut directories=vec![(source.directory,0usize)];while let Some((directory,depth))=directories.pop(){io.original.remaining()?;ensure!(depth<=crate::directory::MAX_PATH_DEPTH,"dependent source path depth bound");
        let raw=graph.metadata.get(&directory).context("dependent source directory absent")?;
        if serde_json::from_slice::<serde_json::Value>(raw)?.get("format_version").and_then(serde_json::Value::as_u64)==Some(2){let mut after=None;
            loop{io.original.remaining()?;let page=PagedDirectory::new(&reader).scan(&directory,after.as_deref(),1024)?;for entry in page.entries{graph.entries=graph.entries.checked_add(1).context("source entry overflow")?;ensure!(graph.entries<=data::MAX_LOCAL_OBJECTS,"dependent source path count bound");
                match entry.node{PagedNode::Directory{object}=>directories.push((object,depth+1)),PagedNode::File{logical_size,..}=>graph.logical_bytes=graph.logical_bytes.checked_add(logical_size).context("source logical bytes overflow")?,PagedNode::Symlink{..}=>{}}
                ensure!(graph.logical_bytes<=data::MAX_LOCAL_BYTES&&directories.len()<=100_000,"dependent source logical/frontier bound");}after=page.next_after;if after.is_none(){break;}}
        }else{let flat:DirectoryManifest=serde_json::from_slice(raw)?;for entry in flat.entries{graph.entries=graph.entries.checked_add(1).context("source entries overflow")?;ensure!(graph.entries<=data::MAX_LOCAL_OBJECTS,"dependent source path count bound");
            match entry.node{EntryNode::Directory{object}=>directories.push((object,depth+1)),EntryNode::File{object,..}=>graph.logical_bytes=graph.logical_bytes.checked_add(object.size_bytes).context("source logical bytes overflow")?,EntryNode::Symlink{..}=>{}}
            ensure!(graph.logical_bytes<=data::MAX_LOCAL_BYTES&&directories.len()<=100_000,"dependent source logical/frontier bound");}}
    }
    environment.check_locked(io,state)?;io.current()?;Ok(graph)
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]pub enum DependentOwnerOrigin{OriginalPublication,ExplicitReattachment}
pub struct EnvironmentDependentPinOwner{store:LocalStore,custody:Arc<StoreCustody>,pin:String,record:DependentPinData,
    environment:Arc<RetainedEnvironmentSubgraph>,local:Arc<BTreeSet<ObjectRef>>,origin:DependentOwnerOrigin}
impl EnvironmentDependentPinOwner{
    pub fn root_data(&self)->&ObjectRef{&self.record.root}
    pub fn record_data(&self)->&DependentPinData{&self.record}
    pub fn environment(&self)->&Arc<RetainedEnvironmentSubgraph>{&self.environment}
    pub fn origin_data(&self)->DependentOwnerOrigin{self.origin}
    pub fn pin_name_data(&self)->&str{&self.pin}
    pub fn store_root_data(&self)->&std::path::Path{self.store.root()}
    pub fn contains_data(&self,r:&ObjectRef,original:&dyn ImageImportFence)->Result<bool>{Ok(self.local.contains(r)||self.environment.contains(r,original)?)}
    pub fn matches_record_data(&self,state:&RetentionState,pin:&str)->bool{self.pin==pin&&state.environment_dependencies.as_ref().and_then(|d|d.pins.get(pin))==Some(&self.record)}
    pub(super) fn check(&self,io:&StoreIo,state:&RetentionState)->Result<()> {io.current()?;ensure!(io.store.root==self.store.root,"dependent actual local store differs");self.custody.current(io)?;
        let data=state.environment_dependencies.as_ref().context("dependent original record absent")?;ensure!(data.pins.get(&self.pin)==Some(&self.record),"dependent original pin changed");
        self.environment.check_locked(io,state)?;let binding=self.environment.binding_data()?;ensure!(binding.id_data()?==self.record.environment_id&&data.environments.get(&self.record.environment_id)==Some(&binding),"dependent original Environment binding differs");io.current()?;Ok(())}
}
pub struct EnvironmentDependentPublication{io:StoreIo,environment:Arc<RetainedEnvironmentSubgraph>,stage:String,pin:String,root:ObjectRef,operation:String,
    before:Vec<Option<crate::store::baked_image::BakedVersionSweep>>,after:Vec<Option<crate::store::baked_image::BakedVersionSweep>>,attempted:bool,ready:bool,
    record:Option<DependentPinData>,ack:Option<Arc<EnvironmentDependentPinOwner>>,failure:Option<anyhow::Error>,#[cfg(test)]fault:Option<TestPoint>}
impl LocalStore{
    pub fn publish_environment_dependent_retaining(&self,out:&mut Option<EnvironmentDependentPublication>,stage:String,pin:String,root:ObjectRef,environment:Arc<RetainedEnvironmentSubgraph>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original dependent publication occupied");*out=Some(EnvironmentDependentPublication{io:StoreIo::staged(self.clone(),original),environment,stage,pin,root,operation:Uuid::new_v4().to_string(),before:Vec::new(),after:Vec::new(),attempted:false,ready:false,record:None,ack:None,failure:None,#[cfg(test)]fault:None});out.as_mut().unwrap().run()
    }
}
impl EnvironmentDependentPublication{
    pub fn acknowledgment(&self)->Result<&Arc<EnvironmentDependentPinOwner>>{ensure!(self.ready,"original dependent publication ACK absent/unknown");self.ack.as_ref().context("actual dependent owner absent")}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"original dependent publication cannot replay");self.attempted=true;if let Err(e)=self.run_inner(){self.failure=Some(e);bail!("dependent publication failed; retain original intents/IO/owners")}Ok(())}
    fn run_inner(&mut self)->Result<()> {
        validate_pin(&self.pin)?;let binding=self.environment.binding_data()?;let id=binding.id_data()?;
        self.environment.check_versions_retaining(&mut self.before,self.io.original.clone())?;self.io.acquire(true)?;let mut state=self.io.read_state()?;self.environment.check_locked(&self.io,&state)?;
        ensure!(state.staging.get(&self.stage).is_some_and(|s|s.expires_at_ms>now_ms().unwrap_or(u64::MAX)),"dependent publication live staging absent");
        ensure!(!state.pins.contains_key(&self.pin)&&!state.backed_retention.as_ref().is_some_and(|p|p.pins.contains_key(&self.pin)),"dependent root conflicts with existing complete-local/backed pin");
        let data=state.environment_dependencies.get_or_insert_with(EnvironmentDependenciesData::default);ensure!(data.pins.len()<data::MAX_DEPENDENT_PINS&&data.holds.len()<data::MAX_DEPENDENT_HOLDS
            &&!data.pins.contains_key(&self.pin)&&!data.holds.values().any(|h|h.pin==self.pin),"dependent root original/unknown publication already exists or capacity reached");
        if let Some(old)=data.environments.get(&id){ensure!(old==&binding,"Environment dependency binding collision");}else{data.environments.insert(id.clone(),binding);}
        data.holds.insert(self.operation.clone(),DependentHoldData{format_version:1,operation_id:self.operation.clone(),pin:self.pin.clone(),root:self.root.clone(),environment_id:id.clone(),staging_id:self.stage.clone()});
        state=self.io.write_state(state)?;#[cfg(test)]if self.fault==Some(TestPoint::AfterHold){bail!("injected dependent hold")}
        self.io.durable_reads=true;let graph=local_graph(&mut self.io,&state,&self.root,&self.environment)?;
        let shards=self.io.shards.values().map(|(f,_)|f.try_clone()).collect::<std::io::Result<Vec<_>>>()?;for shard in shards{self.io.sync_directory(shard)?;}
        self.io.sync_directory(self.io.objects.as_ref().unwrap().try_clone()?)?;self.io.sync_directory(self.io.tmp.as_ref().unwrap().try_clone()?)?;
        self.environment.check_versions_retaining(&mut self.after,self.io.original.clone())?;
        ensure!(state.staging[&self.stage].expires_at_ms>now_ms()?,"dependent original stage expired");
        let record=DependentPinData{format_version:1,operation_id:self.operation.clone(),root:self.root.clone(),environment_id:id,local_graph_sha256:local_digest(&graph)?,local_objects:graph.members.len()as u64,local_bytes:graph.bytes,depth:2,generation:state.generation.checked_add(1).context("dependent generation overflow")?};
        self.record=Some(record.clone());let data=state.environment_dependencies.as_mut().unwrap();data.pins.insert(self.pin.clone(),record.clone());data.holds.remove(&self.operation);state.staging.remove(&self.stage);
        #[cfg(test)]if self.fault==Some(TestPoint::FinalRename){self.io.fail_after_rename=true;}
        state=self.io.write_state(state)?;#[cfg(test)]if self.fault==Some(TestPoint::AfterCommit){bail!("injected dependent late ACK")}
        self.ack=Some(Arc::new(EnvironmentDependentPinOwner{store:self.io.store.clone(),custody:StoreCustody::capture(&self.io)?,pin:self.pin.clone(),record,environment:self.environment.clone(),local:Arc::new(graph.members),origin:DependentOwnerOrigin::OriginalPublication}));
        self.ack.as_ref().unwrap().check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())
    }
}
pub struct EnvironmentDependentRead{owner:Arc<EnvironmentDependentPinOwner>,io:StoreIo,reference:ObjectRef,bytes:Option<Vec<u8>>,attempted:bool,ready:bool}
impl EnvironmentDependentPinOwner{
    pub fn read_retaining(self:&Arc<Self>,out:&mut Option<EnvironmentDependentRead>,reference:ObjectRef,original:Arc<dyn ImageImportFence>)->Result<()> {ensure!(out.is_none(),"original dependent read occupied");
        *out=Some(EnvironmentDependentRead{owner:self.clone(),io:StoreIo::staged(self.store.clone(),original),reference,bytes:None,attempted:false,ready:false});out.as_mut().unwrap().run()}
}
impl EnvironmentDependentRead{
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"original dependent read cannot retry");self.attempted=true;self.io.acquire(false)?;let state=self.io.read_state()?;self.owner.check(&self.io,&state)?;
        self.bytes=Some(if self.owner.local.contains(&self.reference){self.io.read_object(&self.reference,bound(&self.reference,&[])?)?.context("recorded dependent local object absent")?}
            else{ensure!(self.owner.environment.contains(&self.reference,&*self.io.original)?,"read outside actual dependent graph");parent_bytes(&mut self.io,&state,self.owner.environment.parent(),&self.reference)?});
        self.owner.check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())}
    pub fn verified_bytes(&self)->Result<&[u8]>{ensure!(self.ready,"original dependent read incomplete/unknown");self.bytes.as_deref().context("actual dependent bytes absent")}
}
pub struct EnvironmentDependentReattachment{io:StoreIo,pin:String,environment:Arc<RetainedEnvironmentSubgraph>,before:Vec<Option<crate::store::baked_image::BakedVersionSweep>>,after:Vec<Option<crate::store::baked_image::BakedVersionSweep>>,
    owner:Option<Arc<EnvironmentDependentPinOwner>>,attempted:bool,ready:bool,failure:Option<anyhow::Error>}
impl LocalStore{
    pub fn reattach_environment_dependent_retaining(&self,out:&mut Option<EnvironmentDependentReattachment>,pin:String,environment:Arc<RetainedEnvironmentSubgraph>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original dependent reattachment occupied");*out=Some(EnvironmentDependentReattachment{io:StoreIo::staged(self.clone(),original),pin,environment,before:Vec::new(),after:Vec::new(),owner:None,attempted:false,ready:false,failure:None});let s=out.as_mut().unwrap();
        if let Err(e)=s.run(){s.failure=Some(e);bail!("dependent reattachment failed; retain original new owner")};Ok(())}
}
impl EnvironmentDependentReattachment{
    pub fn reattached_owner(&self)->Result<&Arc<EnvironmentDependentPinOwner>>{ensure!(self.ready,"dependent explicit reattachment incomplete");self.owner.as_ref().context("actual dependent reattachment absent")}
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"dependent reattachment cannot retry");self.attempted=true;validate_pin(&self.pin)?;
        self.environment.check_versions_retaining(&mut self.before,self.io.original.clone())?;self.io.acquire(true)?;let state=self.io.read_state()?;self.environment.check_locked(&self.io,&state)?;
        let data=state.environment_dependencies.as_ref().context("saved data alone cannot supply dependent ownership")?;ensure!(data.holds.is_empty(),"unknown dependent publication still held");
        let record=data.pins.get(&self.pin).context("dependent record absent")?.clone();let binding=self.environment.binding_data()?;
        ensure!(binding.id_data()?==record.environment_id&&data.environments.get(&record.environment_id)==Some(&binding),"original Environment parent differs");
        self.io.durable_reads=true;let graph=local_graph(&mut self.io,&state,&record.root,&self.environment)?;
        ensure!(record.local_graph_sha256==local_digest(&graph)?&&record.local_objects==graph.members.len()as u64&&record.local_bytes==graph.bytes,"dependent original local closure differs");
        let shards=self.io.shards.values().map(|(f,_)|f.try_clone()).collect::<std::io::Result<Vec<_>>>()?;for shard in shards{self.io.sync_directory(shard)?;}
        self.io.sync_directory(self.io.objects.as_ref().unwrap().try_clone()?)?;self.io.sync_directory(self.io.tmp.as_ref().unwrap().try_clone()?)?;
        self.environment.check_versions_retaining(&mut self.after,self.io.original.clone())?;
        self.owner=Some(Arc::new(EnvironmentDependentPinOwner{store:self.io.store.clone(),custody:StoreCustody::capture(&self.io)?,pin:self.pin.clone(),record,environment:self.environment.clone(),local:Arc::new(graph.members),origin:DependentOwnerOrigin::ExplicitReattachment}));
        self.owner.as_ref().unwrap().check(&self.io,&state)?;self.io.original.remaining()?;self.io.release_known()?;self.ready=true;Ok(())}
}

pub(in crate::store::backed_retention) fn gc_check(owners:&[Arc<EnvironmentDependentPinOwner>],io:&StoreIo,state:&RetentionState)->Result<()> {
    let expected=state.environment_dependencies.as_ref().map_or(0,|d|d.pins.len());ensure!(owners.len()==expected&&owners.len()<=data::MAX_DEPENDENT_PINS,"actual dependent owner DAG absent/extra; no GC");
    let mut names=BTreeSet::new();for owner in owners{ensure!(names.insert(owner.pin.clone()),"duplicate actual dependent GC owner");owner.check(io,state)?;}
    if let Some(d)=&state.environment_dependencies{ensure!(d.holds.is_empty(),"unknown dependent publication blocks GC");}Ok(())
}
pub(in crate::store::backed_retention) fn gc_local(owners:&[Arc<EnvironmentDependentPinOwner>],io:&mut StoreIo,state:&RetentionState)->Result<BTreeSet<String>> {let mut retained=BTreeSet::new();
    for owner in owners{owner.check(io,state)?;let graph=local_graph(io,state,&owner.record.root,&owner.environment)?;
        ensure!(local_digest(&graph)?==owner.record.local_graph_sha256&&graph.members.len()as u64==owner.record.local_objects&&graph.bytes==owner.record.local_bytes,"dependent GC local graph changed");
        retained.extend(graph.members.into_iter().map(|r|r.digest));}Ok(retained)
}
