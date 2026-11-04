//! Versioned persistent DATA only. No deserialized value supplies a live
//! backing, original ACK, Root role, project epoch or executable permission.
use super::*;

pub(crate) const MAX_BACKINGS:usize=4;
pub(crate) const MAX_BACKED_PINS:usize=1024;
pub(crate) const MAX_BACKED_HOLDS:usize=16;
pub(crate) const MAX_BACKED_REFS:usize=65_536;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BakedInputsBindingData {
    pub format_version:u32,pub inventory:ObjectRef,pub installer_input:ObjectRef,pub manifest:ObjectRef,
    pub closure_sha256:String,pub object_count:u64,pub encoded_bytes:u64,
}
impl BakedInputsBindingData {
    pub fn id_data(&self)->Result<String>{self.validate()?;Ok(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(self)?).digest)}
    pub(crate) fn validate(&self)->Result<()> {
        ensure!(self.format_version==1,"unsupported baked input binding DATA");
        for r in [&self.inventory,&self.installer_input,&self.manifest]{r.validate()?;}
        ensure!(self.inventory.kind==ObjectKind::Blob&&self.inventory.size_bytes>0&&self.inventory.size_bytes<=16*1024*1024
            &&self.installer_input.kind==ObjectKind::Directory&&self.installer_input.size_bytes>0&&self.installer_input.size_bytes<=64*1024
            &&self.manifest.kind==ObjectKind::Blob&&self.manifest.size_bytes>0&&self.manifest.size_bytes<=32*1024*1024
            &&self.closure_sha256.len()==64&&is_lower_hex(&self.closure_sha256)
            &&(1..=MAX_BACKED_REFS as u64).contains(&self.object_count)&&self.encoded_bytes<=2*1024*1024*1024,
            "baked input binding exceeds typed profile");Ok(())
    }
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackedPinData {
    pub format_version:u32,pub operation_id:String,pub root:ObjectRef,pub local_objects:Vec<ObjectRef>,
    pub backing_ids:Vec<String>,pub graph_sha256:String,pub generation:u64,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackedHoldData {
    pub format_version:u32,pub operation_id:String,pub pin:String,pub root:ObjectRef,pub staging_id:String,pub backing_ids:Vec<String>,
}
/// This optional field changes the canonical retention bytes. Old
/// deny_unknown_fields readers refuse it at open/retention mutation/GC. An
/// already-open old handle can still add immutable objects without touching
/// retention; it cannot safely mutate this envelope. Base1/2 and
/// the object-set ledger retain their existing semantics and version checks.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackedRetentionData {
    pub format_version:u32,pub bindings:BTreeMap<String,BakedInputsBindingData>,
    pub pins:BTreeMap<String,BackedPinData>,pub holds:BTreeMap<String,BackedHoldData>,
}
impl Default for BackedRetentionData {fn default()->Self{Self{format_version:1,bindings:BTreeMap::new(),pins:BTreeMap::new(),holds:BTreeMap::new()}}}
fn ids(ids:&[String],bindings:&BTreeMap<String,BakedInputsBindingData>)->Result<()> {
    ensure!(!ids.is_empty()&&ids.len()<=MAX_BACKINGS&&ids.windows(2).all(|w|w[0]<w[1])
        &&ids.iter().all(|id|bindings.contains_key(id)),"backed binding IDs absent/duplicate/unsupported");Ok(())
}
pub(crate) fn validate_retention(state:&RetentionState)->Result<()> {
    super::environment_dependency_data::validate_retention(state)?;
    super::source_dependency_data::validate_retention(state)?;
    let Some(data)=&state.backed_retention else{return Ok(())};
    ensure!(matches!(state.format_version,1|2)&&data.format_version==1&&data.bindings.len()<=MAX_BACKINGS
        &&data.pins.len()<=MAX_BACKED_PINS&&data.holds.len()<=MAX_BACKED_HOLDS,"backed retention DATA profile");
    for(id,binding)in &data.bindings{ensure!(*id==binding.id_data()?,"backed binding key differs");}
    let mut total=0usize;
    for(name,pin)in &data.pins{validate_pin(name)?;pin.root.validate()?;ids(&pin.backing_ids,&data.bindings)?;
        ensure!(!state.pins.contains_key(name)&&pin.format_version==1&&Uuid::parse_str(&pin.operation_id).is_ok()
            &&pin.generation>0&&pin.generation<=state.generation&&pin.graph_sha256.len()==64&&is_lower_hex(&pin.graph_sha256)
            &&pin.local_objects.len()<=MAX_BACKED_REFS&&pin.local_objects.windows(2).all(|w|w[0]<w[1]),"invalid/conflicting backed pin DATA");
        total=total.checked_add(pin.local_objects.len()).context("backed local retention overflow")?;
        ensure!(total<=131_072,"backed local partition count exceeds profile");for r in &pin.local_objects{r.validate()?;}
    }
    for(id,hold)in &data.holds{validate_pin(&hold.pin)?;hold.root.validate()?;ids(&hold.backing_ids,&data.bindings)?;
        ensure!(hold.format_version==1&&id==&hold.operation_id&&Uuid::parse_str(id).is_ok()&&!hold.staging_id.is_empty()
            &&!state.pins.contains_key(&hold.pin)&&!data.pins.contains_key(&hold.pin),"invalid/conflicting backed original hold");}
    ensure!(data.bindings.keys().all(|id|data.pins.values().any(|p|p.backing_ids.contains(id))||data.holds.values().any(|h|h.backing_ids.contains(id))),
        "unreferenced backing DATA requires explicit recovery");
    Ok(())
}
