use super::*;
use crate::{managed_machine_cas::ManagedMachineCas,managed_runtime_image_preseed::ImagePreseedExport};
use rudder_native::engine_registration::FreshProjectPreparation;
/// An environment is immutable DATA backed by actual pins/current image and
/// per-VM input custody. It grants no source/project/task/lease authority.
pub(crate) struct ManagedFreshEnvironment {machine:Arc<ManagedMachineCas>,image:Arc<ImagePreseedExport>,preparation:Arc<FreshProjectPreparation>,
    network:Option<VmNetworkInputs>,statics:Vec<StaticFile>,static_dirs:Vec<Directory>,stage:Option<String>,environment:Option<ObjectRef>,network_directory:Option<ObjectRef>,pin:String,pin_attempted:bool,pin_ack:bool,attempted:bool,fenced:AtomicBool}
impl ManagedFreshEnvironment {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,machine:Arc<ManagedMachineCas>,image:Arc<ImagePreseedExport>,preparation:Arc<FreshProjectPreparation>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        clock(original,cancelled)?;ensure!(out.is_none(),"fresh environment original carrier occupied");
        ensure!(Arc::ptr_eq(machine.registration(),preparation.registration())&&Arc::ptr_eq(&image.source().machine(),&machine),"fresh environment differs from actual image/registered CAS owner");
        let p=preparation.prepare();*out=Some(Self{machine,image,preparation:preparation.clone(),network:None,statics:Vec::new(),static_dirs:Vec::new(),stage:None,environment:None,network_directory:None,
            pin:format!("fresh-env:{}:{}",p.namespace.project_id,p.request_id),pin_attempted:false,pin_ack:false,attempted:false,fenced:AtomicBool::new(false)});
        out.as_mut().unwrap().prepare_retaining(original,cancelled)
    }
    fn prepare_retaining(&mut self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        ensure!(!self.attempted,"fresh environment already attempted");self.attempted=true;clock(original,cancelled)?;self.preparation.assert_current()?;self.machine.assert_custody()?;
        self.image.assert_current_for_fresh(original,cancelled)?;self.network=Some(VmNetworkInputs::capture(original,cancelled)?);
        for (_,path)in rudder_native::image_preseed_data::static_files(){clock(original,cancelled)?;let path=PathBuf::from(path);let before=alias_stamp(&path)?;let target=path.canonicalize()?;let data=self.image.captured_file_data(&target)?.context("declared static runtime target absent from actual full image manifest")?;
            let file=ImageFile::open(&target,16*1024*1024,original,cancelled)?;ensure!(file.sha==data.sha256&&file.stamp.len==data.size_bytes&&alias_stamp(&path)?==before&&path.canonicalize()?==target,"declared static runtime differs from actual image capture");self.statics.push(StaticFile{guest:path,alias:before,target,file});}
        for (_,path)in rudder_native::image_preseed_data::static_directories(){self.static_dirs.push(Directory::open(Path::new(path))?);}

        // Only declared per-VM public resolver/hosts policy is new project DATA.
        // Loader/cert/model/package bytes remain in the SAME actual image graph.
        let store=self.machine.store()?;self.stage=Some(store.begin_staging(std::time::Duration::from_secs(120))?.id);let mut entries=Vec::new();
        for name in ["/etc/hosts","/etc/resolv.conf","/etc/nsswitch.conf"] {
            clock(original,cancelled)?;let file=self.network.as_ref().unwrap().files.iter().find(|f|f.stamp.path==Path::new(name)).context("actual declared VM input absent")?;file.current()?;
            let mut bytes=vec![0;usize::try_from(file.stamp.len)?];ensure!(file.held.read_at(&mut bytes,0)?==bytes.len(),"actual VM input read incomplete");file.current()?;
            let reference=store.put_blob(&bytes)?;entries.push(rudder_engine::store::DirectoryEntry::new(Path::new(name).file_name().unwrap().as_encoded_bytes(),rudder_engine::store::EntryNode::File{object:reference,mode:0o444})?);
        }
        entries.sort_by(|a,b|a.name_hex.cmp(&b.name_hex));let directory=store.put_directory(DirectoryManifest{format_version:1,mode:0o555,entries})?;self.network_directory=Some(directory.clone());
        let empty=store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:Vec::new()})?;
        self.environment=Some(store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2{format_version:2,directory:empty,
            platform:BTreeMap::from([("os".into(),"linux".into()),("arch".into(),std::env::consts::ARCH.into()),("runtime_profile".into(),"fly-managed-image-v1".into())]),
            installer_inputs:vec![self.image.installer_input().clone(),self.image.manifest().clone(),directory],
            provenance:BTreeMap::from([("full_image_manifest_sha256".into(),self.image.manifest().digest.clone()),("vm_network_inputs_sha256".into(),self.network.as_ref().unwrap().digest.clone()),("fresh_prepare_id".into(),self.preparation.prepare().request_id.clone())])})?);
        ensure!(!store.retention()?.pins.contains_key(&self.pin),"original fresh environment pin already exists; retain unknown state");self.pin_attempted=true;store.publish_extending(self.stage.as_ref().unwrap(),&self.pin,self.environment.as_ref().unwrap(),&[self.image.installer_input().clone(),self.image.manifest().clone()])?;self.pin_ack=true;
        self.current(original,cancelled)?;Ok(())
    }
    pub(crate) fn current(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        // A caller's deadline/cancel cannot poison immutable physical custody
        // needed by genuine original cleanup or unrelated later task jobs.
        clock(original,cancelled)?;let result=self.physical_current();if result.is_err(){self.fenced.store(true,Ordering::Release);}result?;clock(original,cancelled)
    }
    fn physical_current(&self)->Result<()> {ensure!(!self.fenced.load(Ordering::Acquire),"original fresh environment custody fenced");self.preparation.assert_current()?;self.machine.assert_custody()?;
        self.image.source().assert_retained_custody()?;if let Some(network)=&self.network{network.current()?;}for item in &self.statics{item.current()?;}for dir in &self.static_dirs{dir.current()?;}
        if self.pin_attempted{ensure!(self.machine.store()?.retention()?.pins.get(&self.pin)==self.environment.as_ref(),"actual fresh environment pin changed");}Ok(())
    }
    pub(crate) fn assert_retained_custody(&self)->Result<()> {ensure!(self.pin_ack&&!self.fenced.load(Ordering::Acquire),"fresh environment original ACK absent/fenced");self.preparation.assert_current()?;self.machine.assert_custody()?;self.image.source().assert_retained_custody()?;self.network.as_ref().context("original VM inputs absent")?.current()?;for item in &self.statics{item.current()?;}for dir in &self.static_dirs{dir.current()?;}ensure!(self.machine.store()?.retention()?.pins.get(&self.pin)==self.environment.as_ref(),"fresh environment pin changed");Ok(())}
    pub(crate) fn environment(&self)->Result<&ObjectRef>{ensure!(self.pin_ack,"fresh environment original pin ACK absent");self.environment.as_ref().context("fresh environment absent")}
    pub(crate) fn machine(&self)->&Arc<ManagedMachineCas>{&self.machine}
    pub(crate) fn image(&self)->&Arc<ImagePreseedExport>{&self.image}
    pub(crate) fn preparation(&self)->&Arc<FreshProjectPreparation>{&self.preparation}
    pub(crate) fn static_mounts(&self)->Vec<ReadOnlyRuntime>{let mut mounts:Vec<_>=self.statics.iter().map(|f|ReadOnlyRuntime{source:f.target.clone(),guest_path:f.guest.display().to_string()}).collect();mounts.extend(self.static_dirs.iter().map(|d|ReadOnlyRuntime{source:d.stamp.path.clone(),guest_path:d.stamp.path.display().to_string()}));mounts}
    pub(crate) fn settings(&self)->Result<jj_lib::settings::UserSettings>{settings_from_config(self.machine.config())}
}

struct StaticFile{guest:PathBuf,alias:Stamp,target:PathBuf,file:ImageFile}
impl StaticFile{fn current(&self)->Result<()>{self.file.current()?;ensure!(alias_stamp(&self.guest)?==self.alias&&self.guest.canonicalize()?==self.target,"original declared static runtime alias changed");Ok(())}}
