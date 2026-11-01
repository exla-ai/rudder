//! Selected runtime/profile consumer of the SAME actual backed environment.
//! Host helper bytes are verified against the actual full image graph; no
//! whole-image import, model default, SDK identity or Root mount grant is made.
use super::*;
fn verify_backed_capture(fresh:&fresh_environment::ManagedFreshEnvironment,files:&[ImageFile],stamps:&[Stamp],hashes:&[(PathBuf,String,u32)],loader:&Option<OsLoaderInputs>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    fresh.current(original,cancelled)?;ensure!(fresh.runtime_backing().is_some(),"selected profile needs actual retained runtime backing");
    for file in files.iter().chain(loader.iter().flat_map(|l|l.files.iter())){clock(original,cancelled)?;let declared=fresh.image().captured_file_data(&file.stamp.path)?.context("host runtime helper input missing from actual full image manifest")?;
        ensure!(file.sha==declared.sha256&&file.stamp.len==declared.size_bytes,"host runtime/profile bytes differ from actual backed image");file.current()?;}
    let declared=fresh.image().captured_package_files()?;let actual:BTreeSet<_>=hashes.iter().filter(|(_,_,mode)|mode&libc::S_IFMT==libc::S_IFREG).map(|(p,_,_)|p.clone()).collect();
    ensure!(actual==declared.iter().map(|f|f.path.clone()).collect(),"host profile package differs from complete backed package inventory");
    for file in declared{clock(original,cancelled)?;let i=hashes.binary_search_by(|(p,_,_)|p.cmp(&file.path)).map_err(|_|anyhow::anyhow!("declared host package file absent"))?;
        let j=stamps.binary_search_by(|s|s.path.cmp(&file.path)).map_err(|_|anyhow::anyhow!("original package stamp absent"))?;
        ensure!(hashes[i].1==file.sha256&&stamps[j].len==file.size_bytes,"actual host package file differs from backed bytes");}
    fresh.current(original,cancelled)
}
fn static_inputs(files:&mut Vec<ImageFile>,anchors:&mut DirectoryAnchors,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    for(_,name)in rudder_native::image_preseed_data::static_files(){let path=Path::new(name).canonicalize()?;if !files.iter().any(|f|f.stamp.path==path){files.push(ImageFile::open_anchored(&path,16*1024*1024,original,cancelled,anchors)?);}}Ok(())
}
impl ManagedProfileInspection{
    pub(crate) fn capture_from_fresh_retaining(out:&mut Option<Self>,fresh:Arc<fresh_environment::ManagedFreshEnvironment>,alias:&str,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        clock(original,cancelled)?;ensure!(out.is_none(),"original backed profile inspection occupied");let config=fresh.machine().config().clone();
        let project=config.data().projects.iter().find(|p|p.alias==alias&&p.prepare==*fresh.preparation().prepare()).context("profile scope differs from original fresh preparation")?;
        ensure!(fresh.runtime_backing().is_some()&&config.data().image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).is_some(),"profile inspection missing actual selected backing");let home=project.home.clone();
        *out=Some(Self{config,files:Vec::new(),anchors:DirectoryAnchors::default(),stamps:Vec::new(),hashes:Vec::new(),home,fresh:Some(fresh),dirs:Vec::new(),loader:None});let s=out.as_mut().unwrap();
        capture_fixed_image(&s.config,&mut s.anchors,&mut s.files,&mut s.dirs,&mut s.stamps,&mut s.hashes,&mut s.loader,original,cancelled)?;
        static_inputs(&mut s.files,&mut s.anchors,original,cancelled)?;
        verify_backed_capture(s.fresh.as_ref().unwrap(),&s.files,&s.stamps,&s.hashes,&s.loader,original,cancelled)?;s.current(original,cancelled)
    }
}
impl ManagedHostRuntime{
    pub(super) fn capture_backed_fresh_host(&mut self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        let fresh=self.fresh.as_ref().context("actual fresh environment absent")?.clone();ensure!(fresh.runtime_backing().is_some(),"actual backed preparation absent");
        self.manifest_hash=capture_fixed_image(&self.config,&mut self.anchors,&mut self.files,&mut self.dirs,&mut self.package_stamps,&mut self.package_hashes,&mut self.loader,original,cancelled)?;
        static_inputs(&mut self.files,&mut self.anchors,original,cancelled)?;
        verify_backed_capture(&fresh,&self.files,&self.package_stamps,&self.package_hashes,&self.loader,original,cancelled)?;
        self.assert_current(original,cancelled)?;self.environment_ack=true;Ok(())
    }
    /// Original admitted factory caller only. Actual task backing/Root owner
    /// chooses the variant; configured projection DATA alone cannot select it.
    pub(crate) fn resources_retaining(&self,source:crate::managed_machine_entry::ManagedSeedTaskSource,backend:HostBackend,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<ManagedTaskResources> {
        let Some(backed)=source.backed_inputs.as_ref()else{return self.resources(source,backend)};
        self.assert_current(original,cancelled)?;backed.check_for_new_task(original,cancelled)?;backed.matches(&source.store,&source.runnable_root,&source.seed_source)?;
        let fresh=self.fresh.as_ref().context("task runtime lacks actual original fresh environment")?;
        ensure!(fresh.runtime_backing().is_some_and(|r|Arc::ptr_eq(r,backed.runtime()))&&self.environment.as_ref()==Some(backed.environment()),"task source and runtime backing differ");
        let actual=backed.runtime().backing()?;let mut runtime=actual.projections_data()?;runtime.extend(backed.runtime().network_mounts(original,cancelled)?);
        let HostRuntimeProfile::FlyManagedImageV1{user_name,user_email}=&self.config.data().runtime_profile;
        let resources=ManagedTaskResources{backed_inputs:source.backed_inputs,expected_integration_head:source.expected_integration_head,store:source.store,runnable_root:source.runnable_root,seed_source:source.seed_source,seed:source.seed,
            programs:self.programs(backend)?,runtime,protected_credential_roots:vec![self.project.home.clone(),self.config.data().state_directory.clone()],endpoint_parent:self.project.endpoints.clone(),native_program:NATIVE.into(),native_guest:format!("{GUEST_PACKAGE}/dist/native/rudder-native"),jj_guest:format!("{GUEST_PACKAGE}/dist/native/rudder-jj"),
            user_name:user_name.clone(),user_email:user_email.clone(),timeout_seconds:24*60*60,preparation_cache:source.preparation_cache};
        self.assert_current(original,cancelled)?;Ok(resources)
    }
}
