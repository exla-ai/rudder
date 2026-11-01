//! Synchronous OFF-App selected build on an actual current source. The host
//! retains this whole campaign outside catch/unwind and on its original worker.
//! This is neither a scheduler, model task, fake Joined nor source publisher.
use super::*;
use crate::managed_initial_source::AcceptedSourceBuildOrigin;
use rudder_engine::{protocol::ObjectRef,build::{BuildInputView,BuildProfile},workspace::{ProjectionBudget,retained_build_input::RetainedRunnableBuildInput}};
use rudder_native::root_storage_bootstrap::runtime_image::{RuntimeImageClient,CompilerSdkClient,RetainedCompilerSdkBuild};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum CurrentSdkBuildState {Captured,SourceSelected,InputQualified,Building,ArtifactPublished,RetainedFailure}
struct BuildCell {original:Option<RetainedCompilerSdkBuild>}
/// Non-Serde owner, issued only by the actual MachineControl method below.
/// Its completed DATA is NOT source acceptance, worker join or job disposal.
pub(crate) struct CurrentSourceSdkCampaign {control:MachineControl,project:ProjectTicket,case:rudder_native::host_ingress::SdkBuildCase,seed:Option<SeedTicket>,cache:Option<SeedCacheTicket>,
    origin:Option<Arc<AcceptedSourceBuildOrigin>>,input:Option<RetainedRunnableBuildInput>,runtime:Option<Arc<RuntimeImageClient>>,sdk:Option<Arc<CompilerSdkClient>>,
    profile:Option<BuildProfile>,builds:Vec<BuildCell>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,phase:CurrentSdkBuildState,
    attempted:bool,error:Option<anyhow::Error>}
impl MachineControl {
    /// Capture BEFORE queue. No IO, thread, RPC or project/source constructor.
    /// The genuine worker must run this off App and remain retained through TLS.
    pub(crate) fn capture_current_sdk_campaign_retaining(&self,out:&mut Option<CurrentSourceSdkCampaign>,project:ProjectTicket,
        original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        self.capture_current_sdk_case_campaign_retaining(out,project,rudder_native::host_ingress::SdkBuildCase::Accepted,original,cancelled)
    }
    pub(crate) fn capture_current_sdk_case_campaign_retaining(&self,out:&mut Option<CurrentSourceSdkCampaign>,project:ProjectTicket,case:rudder_native::host_ingress::SdkBuildCase,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"current source SDK campaign occupied");
        *out=Some(CurrentSourceSdkCampaign{control:self.clone(),project,case,seed:None,cache:None,origin:None,input:None,runtime:None,sdk:None,profile:None,builds:Vec::new(),original,cancelled,phase:CurrentSdkBuildState::Captured,attempted:false,error:None});Ok(())
    }
}
impl CurrentSourceSdkCampaign {
    fn time(&self)->Result<()>{time(&self.original,&self.cancelled)}
    fn off_app(&self)->Result<()> {ensure!(std::thread::current().id()!=self.control.shared.app_thread,"SDK current-source campaign requires actual off-App worker");self.time()}
    fn source_current(&self)->Result<()> {
        self.off_app()?;
        let CurrentSourceAdmission::Ready{seed,cache:_}=self.control.current_source_for_task(&self.project,&self.original)? else{anyhow::bail!("actual accepted source pending/fenced; keep original campaign")};
        let held=self.seed.as_ref().context("original selected source absent")?;
        ensure!(seed.id==held.id&&Arc::ptr_eq(&seed.cell,&held.cell),"SDK source generation changed; no rebinding/replay");
        self.origin.as_ref().context("actual accepted source projection absent")?.assert_current()?;self.time()
    }
    /// Once-only execution. The SAME budget covers pending admission, source
    /// projection, SDK lookup, both jobs (when requested) and result checks.
    /// Every staged carrier remains in self on Err/panic; no hidden retry clock.
    pub(crate) fn run_off_app_retaining(&mut self,repeat_identical:bool)->Result<()> {
        ensure!(!self.attempted,"original SDK campaign cannot retry");self.attempted=true;
        let result=self.run(repeat_identical);
        if let Err(error)=result {self.phase=CurrentSdkBuildState::RetainedFailure;self.error=Some(error);anyhow::bail!("current-source SDK campaign failed; retain source/read/build/worker owners");}Ok(())
    }
    fn run(&mut self,repeat_identical:bool)->Result<()> {
        self.off_app()?;
        loop {self.time()?;match self.control.current_source_for_task(&self.project,&self.original)? {
            CurrentSourceAdmission::Ready{seed,cache}=>{self.seed=Some(seed);self.cache=Some(cache);break;},
            CurrentSourceAdmission::Pending=>std::thread::sleep(self.original.phase_limit(Duration::from_millis(1))?),
            CurrentSourceAdmission::Fenced=>anyhow::bail!("actual source shared admission fenced"),
        }}
        self.phase=CurrentSdkBuildState::SourceSelected;
        {let selected=self.seed.as_ref().unwrap();let source=seeds::lock(&selected.cell,&self.original)?;
            source.source.capture_sdk_build_origin_retaining(&mut self.origin,self.original.clone(),self.cancelled.clone())?;}
        self.source_current()?;
        let origin=self.origin.as_ref().unwrap().clone();let owner=origin.runnable_inputs()?;
        let(_,_,policy)=origin.roots_data()?;let mut limits=ProjectionBudget::default();limits.max_elapsed_ms=u64::try_from(self.original.remaining()?.as_millis().min(30_000))?;
        ensure!(limits.max_elapsed_ms>0,"original SDK projection clock expired");
        RetainedRunnableBuildInput::capture_retaining(&mut self.input,origin.store().clone(),owner,policy,limits,origin.read_fence())?;
        self.source_current()?;check_input(self.input.as_ref().unwrap().input()?,&origin.roots_data()?)?;
        self.phase=CurrentSdkBuildState::InputQualified;self.profile=Some(origin.accepted_profile_case(self.case)?);
        // Actual bootstrap client only. No scope stripping, cached ready JSON,
        // compiler path, bare SDK catalogue or caller-constructed BuildRun.
        loop {self.time()?;
            if self.runtime.is_none(){self.runtime=self.control.runtime_environment_client_off_app(&self.original,&self.cancelled)?;}
            if let Some(runtime)=&self.runtime{
                self.sdk=Some(runtime.compiler_sdk_client()?.context("ordinary runtime lacks its original completed SDK client")?);
                let actual=self.sdk.as_ref().unwrap();actual.current(&self.original)?;actual.require_ordinary_execution()?;break;}

            std::thread::sleep(self.original.phase_limit(Duration::from_millis(1))?);
        }
        let sdk=self.sdk.as_ref().unwrap().clone();let profile=self.profile.as_ref().unwrap().clone();let spec=sdk.preparation_spec_data()?;
        ensure!(profile.platform==spec.platform&&profile.buck2.executable_sha256==spec.toolchains["buck2"].configuration["executable_raw_sha256"],"accepted build profile differs from actual SDK platform/frontend");
        let work_parent=origin.work_parent_data()?;let count=if repeat_identical{2}else{1};
        for _ in 0..count {
            self.source_current()?;origin.assert_current()?;self.time()?;
            self.phase=CurrentSdkBuildState::Building;self.builds.push(BuildCell{original:None});let index=self.builds.len()-1;
            let input=self.input.as_ref().unwrap().input()?.clone();
            sdk.build_selected_source_retaining(&mut self.builds[index].original,&input,&profile,origin.store(),&work_parent,self.original.clone(),self.cancelled.clone(),None)?;
            self.source_current()?;let outcome=self.builds[index].original.as_ref().context("original SDK build carrier absent")?.outcome()?;
            outcome.current(&self.original)?;check_input_outcome(outcome,&input,&sdk)?;
            ensure!(!outcome.warm_reuse(),"ordinary SDK smoke never claims a cache hit");
        }
        if self.builds.len()==2 {let a=self.builds[0].original.as_ref().unwrap().outcome()?;let b=self.builds[1].original.as_ref().unwrap().outcome()?;
            ensure!(a.action_key()==b.action_key()&&a.sdk_environment()==b.sdk_environment()&&a.toolchains()==b.toolchains(),"identical accepted inputs changed semantic build identity");}
        self.source_current()?;self.time()?;self.phase=CurrentSdkBuildState::ArtifactPublished;Ok(())
    }
    pub(crate) fn state_data(&self)->CurrentSdkBuildState{self.phase}
    /// Includes failed originals. The cleanup adapter must NOT infer terminal
    /// failure or successful settlement from Err/outcome absence/error strings.
    pub(crate) fn retained_builds(&self)->impl Iterator<Item=&RetainedCompilerSdkBuild>{self.builds.iter().filter_map(|b|b.original.as_ref())}
    pub(crate) fn failure_data(&self)->Option<String>{self.error.as_ref().map(|e|format!("{e:#}"))}
    /// Artifact DATA only; no current-source/worker/disposal grant is returned.
    pub(crate) fn artifact_roots_data(&self)->Result<Vec<ObjectRef>>{ensure!(self.phase==CurrentSdkBuildState::ArtifactPublished,"SDK campaign lacks original artifact ACK");
        self.builds.iter().map(|b|b.original.as_ref().context("original build absent")?.outcome()?.artifacts()).collect()}
}
fn check_input(input:&BuildInputView,roots:&(ObjectRef,ObjectRef,ObjectRef))->Result<()> {ensure!(input.checkpoint()==&roots.0&&input.source_root()==&roots.1&&input.hardlink_group_count()==0,"qualified SDK input changed actual source/Runnable or unsupported topology");Ok(())}
fn check_input_outcome(outcome:&rudder_native::root_storage_bootstrap::runtime_image::CompilerSdkBuildOutcome,input:&BuildInputView,sdk:&Arc<CompilerSdkClient>)->Result<()> {
    ensure!(Arc::ptr_eq(outcome.qualified_owner(),sdk)&&outcome.source_root()==input.source_root()&&outcome.checkpoint()==input.checkpoint()&&outcome.view_root()==input.view_root()&&outcome.input_topology()==input.hardlinks(),"actual SDK outcome differs from retained current-source input");Ok(())
}

#[path="managed_current_source_sdk_cleanup.rs"]mod cleanup;
pub(crate) use cleanup::CurrentSourceSdkCleanup;
