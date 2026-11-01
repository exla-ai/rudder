//! Child of runtime-profile custody. Uses the SAME deterministic full-image
//! DATA builder as the build helper, adding actual registered/config/actor
//! ownership. DATA bytes alone never construct this retained export.
use super::*;
use crate::{managed_machine_cas::ManagedMachineCas,managed_runtime_image_preseed::ManagedImageBudget};
use rudder_native::image_preseed_data::{ImageDataBuild,ImagePreseedPinData};
use rudder_engine::execution::handoff::{OwnedWorkerJoin,WorkerJoined};
use std::{any::Any,sync::Mutex,thread::{self,ThreadId}};
const CHUNK:usize=16*1024;
#[derive(Clone,Serialize)]
pub(crate) struct ImagePreseedBinding {pub image_id:String,pub profile_id:String,pub manifest_sha256:String,pub manifest:ObjectRef,pub installer_input:ObjectRef}
struct Owner {machine:Arc<ManagedMachineCas>,original:ManagedImageBudget,cancelled:Arc<AtomicBool>,files:Vec<ImageFile>,dirs:Vec<Directory>,anchors:DirectoryAnchors,stamps:Vec<Stamp>,hashes:Vec<(PathBuf,String,u32)>,loader:Option<OsLoaderInputs>,
    identity:String,data:Option<ImageDataBuild>,binding:Option<ImagePreseedBinding>,objects:Vec<ObjectRef>,attempted:bool,ready:bool,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
/// Actual original finite importer+TLS handle/result survives every error.
pub(crate) struct ManagedImagePreseed {owner:Mutex<Owner>,factory:Mutex<Option<OwnedWorkerJoin>>,factory_thread:Mutex<Option<ThreadId>>,owned:AtomicBool,joined:Mutex<Option<WorkerJoined>>}
impl ManagedImagePreseed {
    pub(crate) fn capture(machine:Arc<ManagedMachineCas>,original:ManagedImageBudget,cancelled:Arc<AtomicBool>)->Arc<Self> {
        Arc::new(Self{owner:Mutex::new(Owner{machine,original,cancelled,files:Vec::new(),dirs:Vec::new(),anchors:DirectoryAnchors::default(),stamps:Vec::new(),hashes:Vec::new(),loader:None,identity:String::new(),data:None,binding:None,objects:Vec::new(),attempted:false,ready:false,error:None,panic:None}),factory:Mutex::new(None),factory_thread:Mutex::new(None),owned:AtomicBool::new(false),joined:Mutex::new(None)})
    }
    pub(crate) fn start_retaining(self:&Arc<Self>)->Result<()> {
        let mut slot=self.factory.try_lock().map_err(|_|anyhow::anyhow!("original image factory registration busy"))?;ensure!(slot.is_none()&&!self.owned.load(Ordering::Acquire),"original image factory already registered");let source=self.clone();
        let worker=thread::Builder::new().name("rudder-image-cas-import".into()).spawn(move||{
            while !source.owned.load(Ordering::Acquire){thread::yield_now();}
            let mut held=source.owner.lock().unwrap_or_else(|p|p.into_inner());
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<()> {ensure!(source.factory_thread.lock().unwrap_or_else(|p|p.into_inner()).as_ref()==Some(&thread::current().id()),"image importer is not original factory");held.prepare_retaining()}));
            match result {Ok(Ok(()))=>{},Ok(Err(error))=>held.error=Some(error),Err(panic)=>held.panic=Some(panic)}
        })?;
        *self.factory_thread.lock().unwrap_or_else(|p|p.into_inner())=Some(worker.thread().id());*slot=Some(OwnedWorkerJoin::new(worker));self.owned.store(true,Ordering::Release);Ok(())
    }
    pub(crate) fn poll_export_retaining(self:&Arc<Self>,out:&mut Option<Arc<ImagePreseedExport>>)->Result<bool> {
        ensure!(out.is_none(),"image export already published");let factory=self.factory.lock().unwrap_or_else(|p|p.into_inner());let factory=factory.as_ref().context("actual importer handle absent")?;
        factory.start_join_observer()?;let Some(joined)=factory.try_joined()? else{return Ok(false);};*self.joined.lock().unwrap_or_else(|p|p.into_inner())=Some(joined);
        let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());clock(&held.original,&held.cancelled)?;ensure!(held.ready&&held.error.is_none()&&held.panic.is_none(),"image importer failed/unknown; original owner retained");held.current_deep_for_import()?;
        *out=Some(Arc::new(ImagePreseedExport{source:self.clone(),binding:held.binding.as_ref().unwrap().clone(),objects:held.objects.clone(),cached:Mutex::new(None)}));clock(&held.original,&held.cancelled)?;Ok(true)
    }
    fn current<B:RuntimeReadBudget+?Sized>(&self,original:&B,cancelled:&AtomicBool,deep:bool)->Result<()> {
        clock(original,cancelled)?;ensure!(self.joined.lock().unwrap_or_else(|p|p.into_inner()).is_some(),"original image factory/TLS join absent");let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.ready&&held.error.is_none()&&held.panic.is_none(),"original image import ACK absent");
        if deep{held.current_deep()?;}else{held.current_roots()?;}clock(original,cancelled)
    }
    pub(crate) fn assert_retained_custody(&self)->Result<()> {ensure!(self.joined.lock().unwrap_or_else(|p|p.into_inner()).is_some(),"original image worker/TLS join absent");let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.ready&&held.error.is_none()&&held.panic.is_none(),"original image ACK absent");held.current_roots()}
    pub(crate) fn machine(&self)->Arc<ManagedMachineCas>{self.owner.lock().unwrap_or_else(|p|p.into_inner()).machine.clone()}
    pub(crate) fn roots(&self)->Result<Vec<ObjectRef>> {let held=self.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.ready,"actual image not ready");let b=held.binding.as_ref().unwrap();Ok(vec![b.installer_input.clone(),b.manifest.clone()])}
}
impl Owner {
    fn current_deep_for_import(&self)->Result<()> {
        clock(&self.original,&self.cancelled)?;self.machine.assert_custody()?;self.anchors.check_all_with(||clock(&self.original,&self.cancelled))?;
        for dir in &self.dirs{clock(&self.original,&self.cancelled)?;dir.current()?;}
        for file in &self.files{clock(&self.original,&self.cancelled)?;file.current()?;}
        if let Some(loader)=&self.loader{clock(&self.original,&self.cancelled)?;loader.current()?;}
        for previous in &self.stamps{clock(&self.original,&self.cancelled)?;ensure!(package_stamp(&previous.path)?==*previous,"actual image package changed");}
        if let Some(data)=&self.data{data.check_deep_for_import()?;}
        clock(&self.original,&self.cancelled)
    }
    fn current_roots(&self)->Result<()> {self.machine.assert_custody()?;for dir in &self.dirs{dir.current()?;}for file in &self.files{file.current()?;}if let Some(loader)=&self.loader{loader.current()?;}if let Some(data)=&self.data{data.check_roots()?;}Ok(())}
    fn current_deep(&self)->Result<()> {self.current_roots()?;self.anchors.check_all_with(||Ok(()))?;for previous in &self.stamps{ensure!(package_stamp(&previous.path)?==*previous,"actual image package changed");}if let Some(data)=&self.data{data.check_deep()?;}Ok(())}
    fn prepare_retaining(&mut self)->Result<()> {
        clock(&self.original,&self.cancelled)?;ensure!(!self.attempted,"original image import already attempted");self.attempted=true;self.machine.assert_custody()?;
        self.identity=capture_fixed_image(self.machine.config(),&mut self.anchors,&mut self.files,&mut self.dirs,&mut self.stamps,&mut self.hashes,&mut self.loader,&self.original,&self.cancelled)?;
        let policy=self.machine.config().data().image_preseed.as_ref().context("actual full-image preseed policy unconfigured")?.clone();
        ImageDataBuild::capture_retaining(&mut self.data,self.machine.store()?.clone(),self.original.clone(),self.cancelled.clone())?;
        let data=self.data.as_ref().unwrap();let pin=data.pin_data()?;ensure!(pin.format_version==1&&pin.manifest.kind==ObjectKind::Blob&&pin.manifest.digest==policy.full_manifest_sha256,"captured full image manifest differs from qualified policy");
        // Protected baked DATA must be the exact same builder output. It does
        // not grant readiness; the original server publication is separate.
        let baked=ImageFile::open_anchored(Path::new("/var/lib/rudder-image-preseed-v1/manifest.json"),32*1024*1024,&self.original,&self.cancelled,&mut self.anchors)?;
        ensure!(baked.sha==pin.manifest.digest&&baked.stamp.len==pin.manifest.size_bytes,"baked full manifest differs from actual live image");self.files.push(baked);
        let baked_pin=ImageFile::open_anchored(Path::new("/var/lib/rudder-image-preseed-v1/pin.json"),4096,&self.original,&self.cancelled,&mut self.anchors)?;let mut bytes=vec![0;baked_pin.stamp.len as usize];ensure!(baked_pin.held.read_at(&mut bytes,0)?==bytes.len(),"baked pin read incomplete");
        let expected:ImagePreseedPinData=serde_json::from_slice(&bytes)?;ensure!(canonical_json(&expected)?==bytes&&expected==pin,"baked full image pin differs");self.files.push(baked_pin);
        self.objects=data.objects_for_import()?;self.binding=Some(ImagePreseedBinding{image_id:pin.manifest.digest.clone(),profile_id:self.machine.config().data().runtime_manifest.profile_id.clone(),manifest_sha256:pin.manifest.digest.clone(),manifest:pin.manifest,installer_input:pin.installer_input});
        self.current_deep_for_import()?;clock(&self.original,&self.cancelled)?;self.ready=true;Ok(())
    }
}
/// Complete graph plus actual original importer/join/current-image owners.
pub(crate) struct ImagePreseedExport {source:Arc<ManagedImagePreseed>,binding:ImagePreseedBinding,objects:Vec<ObjectRef>,cached:Mutex<Option<(ObjectRef,Vec<u8>)>>}
impl ImagePreseedExport {
    fn original_image_budget(&self,original:&ManagedImageBudget)->Result<()> {let held=self.source.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(original.constraint()==held.original.constraint(),"original image budget replaced");clock(&held.original,&held.cancelled)}
    pub(crate) fn binding(&self)->&ImagePreseedBinding{&self.binding}
    pub(crate) fn objects(&self)->&[ObjectRef]{&self.objects}
    pub(crate) fn manifest(&self)->&ObjectRef{&self.binding.manifest}
    pub(crate) fn installer_input(&self)->&ObjectRef{&self.binding.installer_input}
    pub(crate) fn source(&self)->&Arc<ManagedImagePreseed>{&self.source}
    pub(crate) fn assert_current(&self,original:&ManagedImageBudget,cancelled:&AtomicBool)->Result<()>{self.original_image_budget(original)?;self.source.current(original,cancelled,false)}
    pub(crate) fn assert_current_deep(&self,original:&ManagedImageBudget,cancelled:&AtomicBool)->Result<()>{self.original_image_budget(original)?;self.source.current(original,cancelled,true)}
    pub(crate) fn assert_current_for_fresh(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{self.source.current(original,cancelled,true)}
    pub(crate) fn assert_custody_for_fresh(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{self.source.current(original,cancelled,false)}
    pub(crate) fn captured_file_data(&self,path:&Path)->Result<Option<rudder_native::image_preseed_data::ImageFileData>>{let held=self.source.owner.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.ready,"actual image export not ready");held.current_roots()?;let files=held.data.as_ref().context("actual image DATA owner absent")?.files_data()?;Ok(files.binary_search_by(|f|f.path.as_os_str().as_encoded_bytes().cmp(path.as_os_str().as_encoded_bytes())).ok().map(|i|files[i].clone()))}
    pub(crate) fn contains_actual_program(&self,backend:HostBackend)->Result<bool>{let held=self.source.owner.lock().unwrap_or_else(|p|p.into_inner());held.current_roots()?;let path=Path::new(ROOT).join(match backend{HostBackend::Codex=>"codex",HostBackend::Claude=>"claude"});Ok(held.ready&&held.files.iter().any(|f|f.stamp.path==path&&f.stamp.mode&0o111!=0))}
    pub(crate) fn read_verified_chunk(&self,reference:&ObjectRef,offset:u64,max_bytes:usize,original:&ManagedImageBudget,cancelled:&AtomicBool)->Result<Vec<u8>>{
        self.assert_current(original,cancelled)?;ensure!(self.objects.binary_search(reference).is_ok()&&(1..=CHUNK).contains(&max_bytes)&&offset<=reference.size_bytes,"image read outside actual retained graph/bounds");
        let mut cached=self.cached.lock().unwrap_or_else(|p|p.into_inner());if cached.as_ref().is_none_or(|(r,_)|r!=reference){let machine=self.source.machine();let bytes=machine.store()?.read_object(reference)?;reference.verify(&bytes)?;*cached=Some((reference.clone(),bytes));}
        let bytes=&cached.as_ref().unwrap().1;let start=usize::try_from(offset)?;let end=start.saturating_add(max_bytes).min(bytes.len());let chunk=bytes[start..end].to_vec();drop(cached);self.assert_current(original,cancelled)?;Ok(chunk)
    }
}
