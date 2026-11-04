//! Versioned DATA for one exact prepared Environment boundary. No serialized
//! record can supply the actual preparation/subgraph owners required to use it.
use super::*;
pub const PROFILE:&str="retained-environment-subgraph-v1";
pub const MAX_LOCAL_OBJECTS:u64=100_000;
pub const MAX_LOCAL_BYTES:u64=384*1024*1024;
pub const MAX_DEPENDENT_PINS:usize=1024;
pub const MAX_DEPENDENT_HOLDS:usize=16;
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct EnvironmentBindingData{pub format_version:u32,pub profile:String,pub boundary:ObjectRef,pub preparation_receipt:ObjectRef,
    pub parent_pin:String,pub parent_root:ObjectRef,pub parent_record_sha256:String,pub closure_sha256:String,pub object_count:u64,pub encoded_bytes:u64,pub depth:u32}
impl EnvironmentBindingData{
    pub fn id_data(&self)->Result<String>{self.validate()?;Ok(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(self)?).digest)}
    fn validate(&self)->Result<()> {for r in [&self.boundary,&self.preparation_receipt,&self.parent_root]{r.validate()?;}validate_pin(&self.parent_pin)?;
        ensure!(self.format_version==1&&self.profile==PROFILE&&self.boundary.kind==ObjectKind::EnvironmentRoot&&self.boundary.size_bytes<=64*1024
            &&self.preparation_receipt==self.parent_root&&self.parent_root.kind==ObjectKind::ActionResult
            &&self.depth==1&&(1..=65_536).contains(&self.object_count)&&self.encoded_bytes<=2*1024*1024*1024,"unsupported prepared environment dependency binding");
        for digest in [&self.parent_record_sha256,&self.closure_sha256]{ensure!(digest.len()==64&&is_lower_hex(digest),"environment dependency digest invalid");}Ok(())}
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct DependentPinData{pub format_version:u32,pub operation_id:String,pub root:ObjectRef,pub environment_id:String,
    pub local_graph_sha256:String,pub local_objects:u64,pub local_bytes:u64,pub depth:u32,pub generation:u64}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct DependentHoldData{pub format_version:u32,pub operation_id:String,pub pin:String,pub root:ObjectRef,pub environment_id:String,pub staging_id:String}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]#[serde(deny_unknown_fields)]
pub struct EnvironmentDependenciesData{pub format_version:u32,pub profile:String,pub environments:BTreeMap<String,EnvironmentBindingData>,
    pub pins:BTreeMap<String,DependentPinData>,pub holds:BTreeMap<String,DependentHoldData>}
impl Default for EnvironmentDependenciesData{fn default()->Self{Self{format_version:1,profile:PROFILE.into(),environments:BTreeMap::new(),pins:BTreeMap::new(),holds:BTreeMap::new()}}}
pub(crate) fn record_digest(record:&backed_data::BackedPinData)->Result<String>{Ok(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(record)?).digest)}
pub(crate) fn validate_retention(state:&RetentionState)->Result<()> {
    let Some(data)=&state.environment_dependencies else{return Ok(())};
    ensure!(data.format_version==1&&data.profile==PROFILE&&data.environments.len()<=4&&data.pins.len()<=MAX_DEPENDENT_PINS&&data.holds.len()<=MAX_DEPENDENT_HOLDS,"environment dependency retention profile bound");
    let parents=state.backed_retention.as_ref().context("environment dependency original flat parent absent")?;
    for(id,binding)in &data.environments{ensure!(*id==binding.id_data()?,"environment dependency key differs");
        let parent=parents.pins.get(&binding.parent_pin).context("environment dependency original parent record absent")?;
        ensure!(parent.root==binding.parent_root&&record_digest(parent)?==binding.parent_record_sha256
            &&parent.local_objects.binary_search(&binding.boundary).is_ok()&&!data.pins.contains_key(&binding.parent_pin),"environment dependency changed parent/boundary or cycle");}
    for(name,record)in &data.pins{validate_pin(name)?;record.root.validate()?;
        ensure!(record.format_version==1&&record.root.kind==ObjectKind::RunnableRoot&&record.root.size_bytes<=64*1024&&record.depth==2
            &&Uuid::parse_str(&record.operation_id).is_ok()&&data.environments.contains_key(&record.environment_id)
            &&(1..=MAX_LOCAL_OBJECTS).contains(&record.local_objects)&&record.local_bytes<=MAX_LOCAL_BYTES
            &&record.local_graph_sha256.len()==64&&is_lower_hex(&record.local_graph_sha256)
            &&record.generation>0&&record.generation<=state.generation&&!state.pins.contains_key(name)&&!parents.pins.contains_key(name),"invalid/conflicting dependent root record");}
    let mut held_pins=BTreeSet::new();for(id,hold)in &data.holds{validate_pin(&hold.pin)?;hold.root.validate()?;
        ensure!(hold.format_version==1&&id==&hold.operation_id&&Uuid::parse_str(id).is_ok()&&hold.root.kind==ObjectKind::RunnableRoot
            &&hold.root.size_bytes<=64*1024&&held_pins.insert(&hold.pin)&&data.environments.contains_key(&hold.environment_id)&&!hold.staging_id.is_empty()&&!state.pins.contains_key(&hold.pin)
            &&!parents.pins.contains_key(&hold.pin)&&!data.pins.contains_key(&hold.pin),"invalid/conflicting dependent publication hold");}
    ensure!(data.environments.keys().all(|id|data.pins.values().any(|p|&p.environment_id==id)||data.holds.values().any(|h|&h.environment_id==id)),"unreferenced environment dependency DATA");Ok(())
}
