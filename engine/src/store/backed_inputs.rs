//! Verified immutable baked graph DATA. The actual readonly descriptors and
//! typed graph stay owned. No Root/image-policy/exec/task permission is issued.
//! Callers retain failed constructors/reads; no retry, Drop IO or local pin.
use super::*;
use super::{backed_data::BakedInputsBindingData,baked_image::{BakedImageCas,BakedImageRead}};
use crate::{content::{decode_file_node,validate_file_child},directory::{self,DirectoryStore,PagedDirectory},tree::HardlinkTopology};
use std::sync::Arc;
const OBJECTS:usize=65_536;
const METADATA:u64=128*1024*1024;

pub struct VerifiedBakedInputs {
    baked:Arc<BakedImageCas>,capture_original:Arc<dyn ImageImportFence>,pending:Option<BakedImageRead>,
    metadata:BTreeMap<ObjectRef,Vec<u8>>,members:BTreeSet<ObjectRef>,binding:Option<BakedInputsBindingData>,ready:bool,attempted:bool,
}
impl VerifiedBakedInputs {
    pub fn capture_retaining(out:&mut Option<Arc<Self>>,baked:Arc<BakedImageCas>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original baked inputs output occupied");
        *out=Some(Arc::new(Self{baked,capture_original:original,pending:None,metadata:BTreeMap::new(),members:BTreeSet::new(),binding:None,ready:false,attempted:false}));
        Arc::get_mut(out.as_mut().unwrap()).context("baked inputs aliased before capture")?.capture()
    }
    fn read_capture(&mut self,reference:&ObjectRef,manifest:bool)->Result<Vec<u8>> {
        self.capture_original.remaining()?;ensure!(self.pending.is_none(),"original baked inputs read retained");
        if manifest{self.baked.read_manifest_retaining(&mut self.pending,self.capture_original.clone())?;}
        else{self.baked.read_object_retaining(&mut self.pending,reference.clone(),self.capture_original.clone())?;}
        ensure!(self.pending.as_ref().unwrap().reference_data()==reference,"baked original read changed reference");
        let raw=self.pending.as_ref().unwrap().verified_bytes()?.to_vec();self.capture_original.remaining()?;self.pending.take();Ok(raw)
    }
    fn capture(&mut self)->Result<()> {
        ensure!(!self.attempted,"original baked inputs cannot retry");self.attempted=true;self.baked.assert_current(&*self.capture_original)?;
        let inventory=self.baked.inventory_data()?;let root=inventory.roots.installer_input.clone();
        let allowed:BTreeSet<_>=inventory.objects.into_iter().collect();let mut pending=vec![root.clone()];let mut work=0u64;
        while let Some(r)=pending.pop(){self.capture_original.remaining()?;r.validate()?;
            ensure!(allowed.contains(&r),"baked typed edge absent from original inventory");
            if !self.members.insert(r.clone()){continue;}ensure!(self.members.len()<=OBJECTS,"baked inputs graph count bound");
            if r.kind!=ObjectKind::Blob {
                ensure!(matches!(r.kind,ObjectKind::Directory|ObjectKind::DirectoryShard|ObjectKind::File)&&r.size_bytes>0&&r.size_bytes<=64*1024,"baked inputs content metadata profile");
                work=work.checked_add(r.size_bytes).context("baked metadata work overflow")?;ensure!(work<=METADATA,"baked metadata work bound");
                let raw=self.read_capture(&r,false)?;pending.extend(metadata_references(r.kind,&raw)?);self.metadata.insert(r,raw);
                ensure!(pending.len()<=OBJECTS*4,"baked inputs frontier bound");
            }else{ensure!(r.size_bytes<=4*1024*1024,"baked ordinary chunk bound");}
        }
        {let reader=Metadata{bytes:&self.metadata,original:&*self.capture_original};validate_edges(&reader,&self.metadata,&self.members)?;}
        let topology={let reader=Metadata{bytes:&self.metadata,original:&*self.capture_original};
            let entry=PagedDirectory::new(&reader).lookup(&root,b"hardlinks.json")?.context("baked inputs require declared hardlinks.json")?;
            let directory::EntryNode::File{object,logical_size,..}=entry.node else{bail!("baked topology not a file")};
            ensure!(object.kind==ObjectKind::Blob&&object.size_bytes==logical_size&&logical_size<=4*1024*1024&&self.members.contains(&object),"baked topology bound/member mismatch");object};
        let raw=self.read_capture(&topology,false)?;
        validate_topology(&self.metadata,&root,&raw,&*self.capture_original)?;
        let manifest=inventory.roots.manifest;let raw=self.read_capture(&manifest,true)?;
        validate_manifest(&raw,&root,&self.members,&self.metadata,&*self.capture_original)?;self.members.insert(manifest.clone());
        let objects:Vec<_>=self.members.iter().cloned().collect();let bytes=objects.iter().try_fold(0u64,|n,r|n.checked_add(r.size_bytes).context("baked closure bytes overflow"))?;
        let binding=BakedInputsBindingData{format_version:1,inventory:self.baked.inventory_reference_data().clone(),installer_input:root,manifest,
            closure_sha256:ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&objects)?).digest,object_count:objects.len()as u64,encoded_bytes:bytes};
        binding.validate()?;self.binding=Some(binding);self.baked.assert_current(&*self.capture_original)?;self.capture_original.remaining()?;self.ready=true;Ok(())
    }
    pub fn binding_data(&self)->Result<&BakedInputsBindingData>{ensure!(self.ready,"baked inputs original validation ACK absent");self.binding.as_ref().context("baked binding absent")}
    pub fn current(&self,original:&dyn ImageImportFence)->Result<()> {original.remaining()?;ensure!(self.ready,"baked inputs constructor incomplete");self.baked.assert_current(original)?;original.remaining()?;Ok(())}
    pub fn contains_data(&self,r:&ObjectRef)->bool{self.ready&&self.members.contains(r)}
    pub fn objects_data(&self)->Result<&BTreeSet<ObjectRef>>{self.binding_data()?;Ok(&self.members)}
    /// Original catalog-version witness only. Payload SHA is still checked on
    /// each served read; callers retain this exact sweep on every failure.
    pub fn check_original_versions_retaining(&self,out:&mut Option<super::baked_image::BakedVersionSweep>,original:Arc<dyn ImageImportFence>)->Result<()> {
        self.current(&*original)?;self.baked.check_original_versions_retaining(out,original)
    }
    #[cfg(test)]pub(crate) fn retained_capture_read_data(&self)->bool{self.pending.is_some()}
    pub(super) fn metadata(&self,r:&ObjectRef)->Result<&[u8]>{self.binding_data()?;self.metadata.get(r).map(Vec::as_slice).context("baked nonpayload metadata not in original graph")}
    pub fn read_retaining(self:&Arc<Self>,out:&mut Option<VerifiedBakedRead>,reference:ObjectRef,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"verified baked original read output occupied");*out=Some(VerifiedBakedRead{source:self.clone(),reference,original,read:None,ready:false,attempted:false});out.as_mut().unwrap().run()
    }
}
pub struct VerifiedBakedRead {source:Arc<VerifiedBakedInputs>,reference:ObjectRef,original:Arc<dyn ImageImportFence>,read:Option<BakedImageRead>,ready:bool,attempted:bool}
impl VerifiedBakedRead {
    fn run(&mut self)->Result<()> {ensure!(!self.attempted,"verified baked read cannot retry");self.attempted=true;self.source.current(&*self.original)?;
        ensure!(self.source.members.contains(&self.reference),"read outside actual verified baked graph");
        if self.reference==self.source.binding_data()?.manifest {self.source.baked.read_manifest_retaining(&mut self.read,self.original.clone())?;}
        else{self.source.baked.read_object_retaining(&mut self.read,self.reference.clone(),self.original.clone())?;}
        self.source.current(&*self.original)?;self.ready=true;Ok(())}
    pub fn verified_bytes(&self)->Result<&[u8]>{ensure!(self.ready,"verified baked read unfinished/unknown");self.read.as_ref().context("actual baked read absent")?.verified_bytes()}
}
pub(super) struct Metadata<'a>{pub bytes:&'a BTreeMap<ObjectRef,Vec<u8>>,pub original:&'a dyn ImageImportFence}
impl DirectoryStore for Metadata<'_>{fn read(&self,r:&ObjectRef)->Result<Vec<u8>>{self.original.remaining()?;Ok(self.bytes.get(r).context("typed child metadata absent")?.clone())}
    fn put(&self,_:ObjectKind,_:&[u8])->Result<ObjectRef>{bail!("verified baked metadata readonly")}}
pub(super) fn validate_edges(reader:&Metadata<'_>,metadata:&BTreeMap<ObjectRef,Vec<u8>>,members:&BTreeSet<ObjectRef>)->Result<()> {
    for(r,raw)in metadata{reader.original.remaining()?;
        if r.kind==ObjectKind::Directory&&serde_json::from_slice::<serde_json::Value>(raw)?.get("format_version").and_then(serde_json::Value::as_u64)==Some(2){PagedDirectory::new(reader).validate_directory(r,OBJECTS as u64)?;}
        for child in metadata_references(r.kind,raw)? {ensure!(members.contains(&child),"typed graph child outside complete union");
            if child.kind==ObjectKind::Blob{continue;}let bytes=metadata.get(&child).context("typed child metadata absent from union")?;
            match r.kind {ObjectKind::Directory|ObjectKind::DirectoryShard=>{
                let paged=r.kind==ObjectKind::DirectoryShard||serde_json::from_slice::<serde_json::Value>(raw)?.get("format_version").and_then(serde_json::Value::as_u64)==Some(2);
                if paged&&matches!(child.kind,ObjectKind::DirectoryShard|ObjectKind::File){directory::validate_child(r.kind,raw,&child,bytes)?;}}
                ObjectKind::File=>{let parent=decode_file_node(raw)?;if child.kind==ObjectKind::File{let node=decode_file_node(bytes)?;
                    for(i,c)in parent.children.iter().enumerate(){if *c==child{validate_file_child(&parent,i,&node)?;}}}},
                ObjectKind::RunnableRoot if matches!(child.kind,ObjectKind::SourceRoot|ObjectKind::EnvironmentRoot)=>crate::snapshot::validate_child(raw,&child,bytes)?,_=>{}}
        }
    }reader.original.remaining()?;Ok(())
}
fn path(raw:&[String])->Result<Vec<Vec<u8>>>{ensure!(!raw.is_empty()&&raw.len()<=128,"backed hardlink path depth");raw.iter().map(|s|{
    let p=hex::decode(s)?;ensure!(!p.is_empty()&&p.len()<=255&&hex::encode(&p)==*s&&!p.contains(&0)&&!p.contains(&b'/')&&p!=b"."&&p!=b"..","backed hardlink path component");Ok(p)}).collect()}
fn validate_topology(metadata:&BTreeMap<ObjectRef,Vec<u8>>,root:&ObjectRef,raw:&[u8],original:&dyn ImageImportFence)->Result<()> {
    let topology:HardlinkTopology=serde_json::from_slice(raw)?;ensure!(topology.format_version==1&&canonical_json(&topology)?==raw&&topology.groups.len()<=100_000,"baked topology schema/bound");
    let reader=Metadata{bytes:metadata,original};let tree=PagedDirectory::new(&reader);let mut seen=BTreeSet::new();let mut prior=None;
    for group in topology.groups{original.remaining()?;ensure!((2..=100_000).contains(&group.paths_hex.len()),"baked hardlink group bound");let first=path(&group.paths_hex[0])?;
        ensure!(prior.as_ref().is_none_or(|p|p<&first),"baked hardlink groups not sorted");prior=Some(first);let mut previous=None;let mut expected=None;
        for raw in group.paths_hex{original.remaining()?;let p=path(&raw)?;ensure!(previous.as_ref().is_none_or(|last|last<&p)&&seen.insert(p.clone())&&seen.len()<=1_000_000,"baked hardlink duplicate/order/bound");previous=Some(p.clone());
            let mut directory=root.clone();let mut actual=None;
            for(i,name)in p.iter().enumerate(){let entry=tree.lookup(&directory,name)?.context("baked hardlink path absent")?;match entry.node {
                directory::EntryNode::Directory{object} if i+1<p.len()=>directory=object,
                directory::EntryNode::File{object,logical_size,mode} if i+1==p.len()=>actual=Some((object,logical_size,mode)),_=>bail!("baked hardlink does not end in declared file")}}
            let actual=actual.context("baked hardlink member empty")?;if let Some(expected)=&expected{ensure!(expected==&actual,"baked hardlink contents/size/mode differ");}else{expected=Some(actual);}
        }
    }original.remaining()?;Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {format_version:u32,installer_input:ObjectRef,files:Vec<ManifestFile>}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {path:PathBuf,sha256:String,size_bytes:u64,content:ObjectRef}
fn validate_manifest(raw:&[u8],root:&ObjectRef,members:&BTreeSet<ObjectRef>,metadata:&BTreeMap<ObjectRef,Vec<u8>>,original:&dyn ImageImportFence)->Result<()> {
    let value:serde_json::Value=serde_json::from_slice(raw)?;ensure!(canonical_json(&value)?==raw,"baked full manifest not canonical");let data:Manifest=serde_json::from_value(value)?;
    ensure!(data.format_version==1&&data.installer_input==*root&&data.files.len()<=65_536,"baked manifest root/profile");let mut previous:Option<PathBuf>=None;
    for file in data.files{original.remaining()?;file.content.validate()?;ensure!(file.path.is_absolute()&&!file.path.as_os_str().as_bytes().contains(&0)&&previous.as_ref().is_none_or(|p|p.as_os_str().as_bytes()<file.path.as_os_str().as_bytes())
            &&file.sha256.len()==64&&is_lower_hex(&file.sha256)&&members.contains(&file.content),"baked manifest file/order/member mismatch");previous=Some(file.path);
        let size=match file.content.kind{ObjectKind::Blob=>file.content.size_bytes,ObjectKind::File=>decode_file_node(metadata.get(&file.content).context("baked manifest FileNode absent")?)?.logical_size,_=>bail!("manifest content not a file")};
        ensure!(size==file.size_bytes,"baked manifest logical length differs");
        // Whole logical-file SHA in this manifest is DATA. Actual byte reads
        // still verify raw CAS SHA; this does not invent the emitter's ACK.
    }original.remaining()?;Ok(())
}

#[path="backed_compiler_inputs.rs"]
mod compiler_inputs;
pub use compiler_inputs::{CompilerFileBindingData,CompilerSdkSemanticInputs};
