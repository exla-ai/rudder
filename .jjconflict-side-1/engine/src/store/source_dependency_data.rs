//! Persisted dependencies are DATA, never source/project/Root authority.
use super::*;
use super::environment_dependency_data::{MAX_DEPENDENT_HOLDS,MAX_DEPENDENT_PINS,MAX_LOCAL_BYTES,MAX_LOCAL_OBJECTS};
pub const SOURCE_OVERLAY_PROFILE:&str="retained-source-overlay-v1";
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct SourceParentBindingData{
    pub format_version:u32,pub profile:String,pub parent_pin:String,pub parent_record_sha256:String,
    pub runnable:ObjectRef,pub source:ObjectRef,pub directory:ObjectRef,pub hardlinks:ObjectRef,pub projection_policy:ObjectRef,
    pub environment_id:String,pub local_graph_sha256:String,pub local_objects:u64,pub local_bytes:u64,
    pub source_entries:u64,pub source_logical_bytes:u64,pub source_height:u64,pub depth:u32,
}
impl SourceParentBindingData{
    pub fn id_data(&self)->Result<String>{self.validate()?;Ok(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(self)?).digest)}
    fn validate(&self)->Result<()> {
        ensure!(self.format_version==1&&self.profile==SOURCE_OVERLAY_PROFILE&&self.depth==2,"unsupported source parent profile");validate_pin(&self.parent_pin)?;
        for(r,kind)in [(&self.runnable,ObjectKind::RunnableRoot),(&self.source,ObjectKind::SourceRoot),(&self.directory,ObjectKind::Directory),
            (&self.hardlinks,ObjectKind::Blob),(&self.projection_policy,ObjectKind::Blob)]{r.validate()?;ensure!(r.kind==kind&&r.size_bytes<=if kind==ObjectKind::Blob{4*1024*1024}else{64*1024},"source parent ref kind/size differs");}
        for h in [&self.parent_record_sha256,&self.environment_id,&self.local_graph_sha256]{ensure!(h.len()==64&&is_lower_hex(h),"source parent binding digest invalid");}
        ensure!((1..=MAX_LOCAL_OBJECTS).contains(&self.local_objects)&&self.local_bytes<=MAX_LOCAL_BYTES
            &&self.source_entries<=MAX_LOCAL_OBJECTS&&self.source_logical_bytes<=MAX_LOCAL_BYTES&&self.source_height<=crate::directory::MAX_PATH_DEPTH as u64,
            "source parent exceeds original source bounds");Ok(())
    }
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct SourceOverlayPinData{pub format_version:u32,pub operation_id:String,pub root:ObjectRef,pub parent_id:String,
    pub delta_sha256:String,pub delta_objects:u64,pub delta_bytes:u64,pub source_entries:u64,pub source_logical_bytes:u64,pub source_height:u64,pub depth:u32,pub generation:u64}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct SourceOverlayHoldData{pub format_version:u32,pub operation_id:String,pub pin:String,pub root:ObjectRef,pub parent_id:String,pub staging_id:String}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct SourceDependenciesData{pub format_version:u32,pub profile:String,pub parents:BTreeMap<String,SourceParentBindingData>,pub pins:BTreeMap<String,SourceOverlayPinData>,pub holds:BTreeMap<String,SourceOverlayHoldData>}
impl Default for SourceDependenciesData{fn default()->Self{Self{format_version:1,profile:SOURCE_OVERLAY_PROFILE.into(),parents:BTreeMap::new(),pins:BTreeMap::new(),holds:BTreeMap::new()}}}
pub(crate) fn validate_retention(state:&RetentionState)->Result<()> {
    let Some(data)=&state.source_dependencies else{return Ok(())};
    let base=state.environment_dependencies.as_ref().context("source dependency actual flat parent namespace absent")?;
    let backed=state.backed_retention.as_ref().context("source dependency Environment namespace absent")?;
    ensure!(data.format_version==1&&data.profile==SOURCE_OVERLAY_PROFILE&&data.parents.len()<=MAX_DEPENDENT_PINS
        &&base.pins.len().checked_add(data.pins.len()).is_some_and(|n|n<=MAX_DEPENDENT_PINS)
        &&base.holds.len().checked_add(data.holds.len()).is_some_and(|n|n<=MAX_DEPENDENT_HOLDS),"combined source dependency capacity exceeds original bounds");
    for(id,parent)in &data.parents{
        ensure!(*id==parent.id_data()?,"source parent key differs");
        let original=base.pins.get(&parent.parent_pin).context("source parent original flat pin absent; overlay cannot be parent")?;
        ensure!(original.depth==2&&original.root==parent.runnable&&original.environment_id==parent.environment_id
            &&ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(original)?).digest==parent.parent_record_sha256
            &&original.local_graph_sha256==parent.local_graph_sha256&&original.local_objects==parent.local_objects&&original.local_bytes==parent.local_bytes,
            "source parent original record differs");
        ensure!(!data.pins.contains_key(&parent.parent_pin),"source overlay ancestry/cycle forbidden");
    }
    for(name,pin)in &data.pins{
        validate_pin(name)?;pin.root.validate()?;let parent=data.parents.get(&pin.parent_id).context("source overlay parent binding absent")?;
        ensure!(pin.format_version==1&&Uuid::parse_str(&pin.operation_id).is_ok()&&pin.root.kind==ObjectKind::RunnableRoot&&pin.root.size_bytes<=64*1024
            &&pin.depth==3&&pin.generation>0&&pin.generation<=state.generation&&pin.delta_sha256.len()==64&&is_lower_hex(&pin.delta_sha256)
            &&parent.local_objects.checked_add(pin.delta_objects).is_some_and(|n|n<=MAX_LOCAL_OBJECTS)
            &&parent.local_bytes.checked_add(pin.delta_bytes).is_some_and(|n|n<=MAX_LOCAL_BYTES)
            &&pin.source_entries<=MAX_LOCAL_OBJECTS&&pin.source_logical_bytes<=MAX_LOCAL_BYTES&&pin.source_height<=crate::directory::MAX_PATH_DEPTH as u64
            &&!state.pins.contains_key(name)&&!base.pins.contains_key(name)&&!backed.pins.contains_key(name)
            &&!base.holds.values().any(|h|h.pin==*name)&&!backed.holds.values().any(|h|h.pin==*name),"invalid/conflicting source overlay pin");
    }
    let mut names=BTreeSet::new();for(id,hold)in &data.holds{
        validate_pin(&hold.pin)?;hold.root.validate()?;
        ensure!(hold.format_version==1&&*id==hold.operation_id&&Uuid::parse_str(id).is_ok()&&hold.root.kind==ObjectKind::RunnableRoot
            &&hold.root.size_bytes<=64*1024&&data.parents.contains_key(&hold.parent_id)&&!hold.staging_id.is_empty()&&names.insert(&hold.pin)
            &&!state.pins.contains_key(&hold.pin)&&!base.pins.contains_key(&hold.pin)&&!backed.pins.contains_key(&hold.pin)&&!data.pins.contains_key(&hold.pin)
            &&!base.holds.values().any(|h|h.pin==hold.pin)&&!backed.holds.values().any(|h|h.pin==hold.pin),"invalid source overlay hold");
    }
    ensure!(data.parents.keys().all(|id|data.pins.values().any(|p|&p.parent_id==id)||data.holds.values().any(|h|&h.parent_id==id)),"unreferenced source dependency parent DATA");Ok(())
}
