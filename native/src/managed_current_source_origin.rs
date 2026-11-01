//! Actual accepted-source custody projected by its original native owner.
//! No JSON/root/path constructor, source write, new owner epoch or SDK grant.
use super::*;
use fs2::FileExt;
use rudder_engine::{build::BuildProfile,store::{ImageImportFence,environment_dependencies::RetainedRunnableInputs}};
use std::{sync::{Mutex,MutexGuard,TryLockError},time::Duration};

#[derive(Clone)]
struct Binding {head:GitCommitId,runnable:ObjectRef,source:ObjectRef,seed:ObjectRef,runnable_pin:String,seed_pin:String,
    directory:ObjectRef,policy:ObjectRef,acceptance_lock:PathBuf,work_parent:PathBuf}
struct State {journal:Option<Journal>,cas:Option<DirectoryCustody>,project:Option<DirectoryCustody>,integration:Option<DirectoryCustody>,
    lock:Option<File>,lock_owned:bool,lock_attempted:bool,binding:Option<Binding>,retention:Option<RetainedRunnableInputs>,
    backing:Option<Arc<task_backing::ManagedBackedTaskInputs>>,verified:Option<Arc<rudder_vcs_bridge::projection::VerifiedSourceProjection>>,
    repository:Option<Arc<jj_lib::repo::ReadonlyRepo>>,cache:Option<Arc<rudder_vcs_bridge::seed::RetainedSeedCasCache>>,ready:bool}
/// The whole seed ticket is separately retained by the machine campaign. These
/// are actual aliases of its held lease/directories/verifier/backing, never a
/// reconstructed pin or a caller callback that can authorize arbitrary roots.
pub(crate) struct AcceptedSourceBuildOrigin {lease:EngineSourceLease,store:LocalStore,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    _fresh:Option<Arc<crate::managed_fresh_genesis::ManagedFreshGenesis>>,_accepted:Option<Arc<crate::managed_checkpoint_accept::AcceptedSourceInput>>,
    _paged:Arc<Mutex<Option<rudder_vcs_bridge::seed::PagedSeedGeneration>>>,_git:Arc<Mutex<Option<rudder_vcs_bridge::accepted_git_import::RetainedGitImport>>>,state:Mutex<State>}
impl ManagedInitialSourceOwner {
    pub(crate) fn capture_sdk_build_origin_retaining(&self,out:&mut Option<Arc<AcceptedSourceBuildOrigin>>,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"accepted source build origin already staged");
        *out=Some(Arc::new(AcceptedSourceBuildOrigin{lease:self.lease.clone(),store:self.store.clone(),original,cancelled,
            _fresh:self.fresh.clone(),_accepted:self.accepted_input.clone(),_paged:self.paged.clone(),_git:self.git_import.clone(),state:Mutex::new(State{journal:None,cas:None,project:None,integration:None,lock:None,lock_owned:false,lock_attempted:false,binding:None,retention:None,backing:None,verified:None,repository:None,cache:None,ready:false})}));
        let held=out.as_ref().unwrap();held.capture(self)
    }
}
impl AcceptedSourceBuildOrigin {
    fn lock(&self)->Result<MutexGuard<'_,State>>{loop{self.check_clock()?;match self.state.try_lock(){Ok(s)=>return Ok(s),Err(TryLockError::WouldBlock)=>std::thread::sleep(self.original.phase_limit(Duration::from_millis(1))?),Err(_)=>anyhow::bail!("accepted build origin poisoned; original custody retained")}}}
    fn check_clock(&self)->Result<Duration>{if self.cancelled.load(Ordering::Acquire){self.original.cancel();}self.original.remaining()}
    /// Safe under existing CAS/GC locks: NO SQL/filesystem/CAS or owner callbacks.
    /// Actual held owner/Journal/head/pin checks bracket each external operation.
    pub(crate) fn read_fence(self:&Arc<Self>)->Arc<dyn ImageImportFence>{Arc::new(ReadFence(self.clone()))}
    fn capture(&self,source:&ManagedInitialSourceOwner)->Result<()> {
        let mut state=self.lock()?;let prepared=source.prepared_current(&self.original,&self.cancelled)?;
        ensure!(prepared.baseline.commit==source.head&&prepared.verified.receipt().source==prepared.source_root&&prepared.verified.receipt().tree==prepared.baseline.tree,"actual source mapping/head differs");
        let integration=source.integration.as_ref().context("original source integration directory absent")?;
        state.binding=Some(Binding{head:source.head.clone(),runnable:prepared.runnable_root.clone(),source:prepared.source_root.clone(),seed:prepared.seed_root.clone(),runnable_pin:prepared.runnable_pin.clone(),seed_pin:prepared.seed_pin.clone(),directory:prepared.verified.receipt().directory.clone(),policy:prepared.verified.receipt().projection_policy.clone(),acceptance_lock:integration.path.join("rudder-acceptance.lock"),work_parent:source.project.binding().directory.clone()});
        state.verified=Some(prepared.verified.clone());state.repository=Some(prepared.repository().clone());state.cache=Some(prepared.seed_cache().clone());state.retention=prepared.runnable_inputs();
        state.cas=Some(source.cas_anchor.clone_held()?);state.project=Some(source.project_anchor.clone_held()?);state.integration=Some(integration.clone_held()?);
        // Existing writer-open has a fixed <=100ms busy policy. Refuse a shorter
        // remaining window before that effect instead of silently resetting it.
        ensure!(self.check_clock()?>=Duration::from_millis(100),"original source clock cannot admit existing Journal open");
        state.journal=Some(source.lease.open_source_writer()?);self.check_clock()?;
        let path=state.binding.as_ref().unwrap().acceptance_lock.clone();state.lock_attempted=true;
        state.lock=Some(OpenOptions::new().read(true).write(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(&path)?);
        loop{self.check_clock()?;match FileExt::try_lock_shared(state.lock.as_ref().unwrap()){
            Ok(())=>{state.lock_owned=true;break;},Err(e)if e.kind()==std::io::ErrorKind::WouldBlock=>std::thread::sleep(self.original.phase_limit(Duration::from_millis(1))?),Err(e)=>return Err(e.into())}}
        self.check_state(&state)?;
        if state.retention.is_some(){source.capture_task_backing_retaining(&mut state.backing,&self.original,&self.cancelled)?;ensure!(state.backing.is_some(),"actual retained Runnable lost native runtime/source backing");}
        source.prepared_current(&self.original,&self.cancelled)?;self.check_state(&state)?;state.ready=true;Ok(())
    }
    fn check_state(&self,state:&State)->Result<()> {
        self.check_clock()?;self.lease.wait_shared_current(&self.original,&self.cancelled)?;
        for directory in [&state.cas,&state.project,&state.integration]{directory.as_ref().context("accepted source original directory absent")?.check()?;}
        let b=state.binding.as_ref().context("accepted source binding absent")?;let journal=state.journal.as_ref().context("accepted source original Journal absent")?;
        self.lease.check_journal(journal)?;check_head(journal,self.lease.owner(),&b.head)?;
        let lock=state.lock.as_ref().context("accepted source original lock absent")?;let held=lock.metadata()?;let named=fs::symlink_metadata(&b.acceptance_lock)?;
        ensure!(state.lock_owned&&held.is_file()&&named.is_file()&&!named.file_type().is_symlink()&&held.nlink()==1&&held.uid()==unsafe{libc::geteuid()}&&held.mode()&0o7777==0o600
            &&(held.dev(),held.ino(),held.uid(),held.gid(),held.mode())==(named.dev(),named.ino(),named.uid(),named.gid(),named.mode()),"original shared acceptance lock replaced/unowned");
        let retention=self.store.retention()?;let runnable=if let Some(actual)=&state.retention{actual.root_data()==&b.runnable&&actual.matches_record_data(&retention,&b.runnable_pin)}else{retention.pins.get(&b.runnable_pin)==Some(&b.runnable)};
        ensure!(runnable&&retention.pins.get(&b.seed_pin)==Some(&b.seed),"accepted source/seed original pins changed");
        if let Some(backing)=&state.backing{backing.check_for_new_task(&self.original,&self.cancelled)?;backing.matches(&self.store,&b.runnable,&b.source)?;}
        if let Some(input)=&self._accepted{input.check_current(&self.lease,journal,&self.original,&self.cancelled)?;}
        self.check_clock()?;Ok(())
    }
    pub(crate) fn assert_current(&self)->Result<()> {let state=self.lock()?;ensure!(state.ready,"original accepted build source incomplete");self.check_state(&state)}
    pub(crate) fn runnable_inputs(&self)->Result<RetainedRunnableInputs>{let state=self.lock()?;ensure!(state.ready,"accepted source origin incomplete");self.check_state(&state)?;
        state.retention.clone().context("complete-local current source has no bounded retained projection API; no legacy fallback")}
    pub(crate) fn roots_data(&self)->Result<(ObjectRef,ObjectRef,ObjectRef)>{let state=self.lock()?;ensure!(state.ready,"accepted source incomplete");let b=state.binding.as_ref().unwrap();Ok((b.runnable.clone(),b.source.clone(),b.policy.clone()))}
    pub(crate) fn store(&self)->&LocalStore{&self.store}
    pub(crate) fn work_parent_data(&self)->Result<PathBuf>{let state=self.lock()?;ensure!(state.ready,"accepted source incomplete");Ok(state.binding.as_ref().unwrap().work_parent.clone())}
    pub(crate) fn accepted_profile(&self)->Result<BuildProfile>{self.accepted_profile_case(rudder_native::host_ingress::SdkBuildCase::Accepted)}
    pub(crate) fn accepted_profile_case(&self,case:rudder_native::host_ingress::SdkBuildCase)->Result<BuildProfile>{self.assert_current()?;
        use rudder_native::host_ingress::{SdkBuildCase,SdkInitialSourceProfile};
        let path:&[u8]=match case{SdkBuildCase::Accepted=>b"rudder.toml",SdkBuildCase::ProcDenied|SdkBuildCase::JobDenied=>{
            let source=self._fresh.as_ref().context("operator denial case needs actual fresh source capsule")?;let env=source.environment()?;let config=env.machine().config();
            ensure!(config.data().sdk_initial_source==Some(SdkInitialSourceProfile::FixedSuiteV1)&&config.data().runtime_qualification.is_none()&&config.data().compiler_qualification.is_none(),"operator denial profile not in original ordinary config");
            if case==SdkBuildCase::ProcDenied{b"sdk-probes/proc.toml"}else{b"sdk-probes/job.toml"}}};
        let b=self.lock()?.binding.as_ref().unwrap().clone();let profile=read_accepted_profile_at(&self.store,&b.directory,path,&self.original,&self.cancelled)?;self.assert_current()?;Ok(profile)}
}
struct ReadFence(Arc<AcceptedSourceBuildOrigin>);
impl ImageImportFence for ReadFence{fn remaining(&self)->Result<Duration>{let held=&self.0;held.check_clock()?;held.lease.check_active_for_adoption()?;held.check_clock()}}
fn check_head(journal:&Journal,owner:&rudder_engine::journal::OwnerFence,head:&GitCommitId)->Result<()> {let actual=journal.project_state(&owner.project_id)?;ensure!(actual.owner_active&&actual.owner==*owner&&actual.integration_head==head.0,"accepted SDK source Journal owner/head changed");Ok(())}
fn read_accepted_profile(store:&LocalStore,directory:&ObjectRef,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<BuildProfile>{
    read_accepted_profile_at(store,directory,b"rudder.toml",original,cancelled)
}
fn read_accepted_profile_at(store:&LocalStore,directory:&ObjectRef,path:&[u8],original:&FinalizationBudget,cancelled:&AtomicBool)->Result<BuildProfile>{
    if cancelled.load(Ordering::Acquire){original.cancel();}original.remaining()?;
    let entry=rudder_engine::tree::lookup(store,directory,path)?.context("actual accepted source lacks rudder.toml; no default build policy")?;
    let rudder_engine::directory::EntryNode::File{object,logical_size,..}=entry.node else{anyhow::bail!("accepted rudder.toml is not a regular immutable source file")};
    ensure!(logical_size>0&&logical_size<=128*1024,"accepted build policy byte bound");let bytes=rudder_engine::content::read_file_range(store,&object,0,usize::try_from(logical_size)?)?;
    ensure!(bytes.len()as u64==logical_size,"accepted build policy incomplete");let profile=BuildProfile::from_toml(std::str::from_utf8(&bytes)?)?;
    if cancelled.load(Ordering::Acquire){original.cancel();}original.remaining()?;Ok(profile)
}

#[cfg(test)]#[path="managed_current_source_origin_tests.rs"]mod tests;
