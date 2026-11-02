//! Actual original Joined -> accepted policy -> qualified SDK -> Journal/jj
//! publication. All work is OFF App, retained outside catch/unwind. This module
//! is a completion consumer, never a task scheduler or a JSON proof factory.
use super::*;
use rudder_engine::{build::{BuildInputView,BuildProfile},execution::finalization_budget::FinalizationBudget,
    journal::CanonicalDecisionSnapshot,store::{ImageImportFence,environment_dependencies::RetainedRunnableInputs},
    workspace::{ProjectionBudget as InputBudget,QualifiedRunnableBuildInput,retained_build_input::RetainedRunnableBuildInput}};
use rudder_native::root_storage_bootstrap::runtime_image::{CompilerSdkClient,RetainedCompilerSdkBuild};
use std::{sync::atomic::{AtomicBool,Ordering},os::fd::{AsRawFd,FromRawFd},ffi::CString};
#[path="managed_canonical_policy.rs"]mod policy;
use policy::RetainedCanonicalPolicy;
#[path="managed_canonical_repository.rs"]mod repository;
pub(super) use repository::RetainedIntegrationOpen;

fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()>{if cancel.load(Ordering::Acquire){original.cancel();}original.remaining()?;Ok(())}
struct Fence{lease:EngineSourceLease,original:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl ImageImportFence for Fence{fn remaining(&self)->Result<Duration>{time(&self.original,&self.cancelled)?;loop{self.original.remaining()?;match self.lease.check_active_for_adoption(){Ok(())=>return self.original.remaining(),
    Err(e)if e.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some()=>std::thread::sleep(self.original.phase_limit(Duration::from_millis(1))?),Err(e)=>return Err(e)}}}}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]pub(crate) enum CanonicalState{Captured,Snapshot,Gates,Intent,Publishing,Published,RetainedFailure,Recovery}
pub(super) struct PublicationPermit; // born before actual transfer; spent BEFORE intent/jj writes.
struct BuildCell{pending:Option<RetainedCompilerSdkBuild>,ready:Option<Arc<RetainedCompilerSdkBuild>>,terminal:Option<Arc<RetainedCompilerSdkBuild>>}
/// Only actual SDK outcomes construct this; no Clone/Serde/BuildRun constructor.
pub(crate) struct CanonicalGateEvidence{sdk:Arc<CompilerSdkClient>,builds:Vec<Arc<RetainedCompilerSdkBuild>>,snapshot:Arc<CanonicalDecisionSnapshot>,
    assignment:WorkspaceAssignment,candidate:ManagedCheckpointCandidate,binding:AttemptBinding,policy_record:ObjectRef,data:IntegrationEvidence}
impl CanonicalGateEvidence{
    fn current(&self,a:&ManagedCheckpointAcceptance,journal:&Journal,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;a.lease.wait_shared_current(original,cancel)?;a.check(journal)?;
        ensure!(a.completion.assignment()==&self.assignment&&a.candidate.as_ref().is_some_and(|c|c.source==self.candidate.source&&c.runnable==self.candidate.runnable&&c.environment==self.candidate.environment&&c.directory==self.candidate.directory&&c.hardlinks==self.candidate.hardlinks)
            &&a.binding.as_ref()==Some(&self.binding),"canonical evidence belongs to another original candidate/task");
        self.snapshot.assert_current(journal,original,cancel)?;self.execution_current(original,cancel)?;
        ensure!(self.data.tested_head==self.binding.baseline.commit.0&&self.data.tested_source_root==self.candidate.source&&self.data.tested_environment_root==self.candidate.environment,"canonical runtime evidence binding differs");
        time(original,cancel)
    }
    fn execution_current(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;self.sdk.current(original)?;
        for build in &self.builds{let outcome=build.outcome()?;outcome.current(original)?;ensure!(Arc::ptr_eq(outcome.qualified_owner(),&self.sdk)&&outcome.source_root()==&self.candidate.source
            &&outcome.view_root()==&self.candidate.directory&&outcome.input_topology()==Some(&self.candidate.hardlinks)&&outcome.checkpoint()==&self.candidate.runnable,"actual compiler result differs from original canonical candidate");}
        time(original,cancel)
    }
    pub(crate) fn builds(&self)->&[Arc<RetainedCompilerSdkBuild>]{&self.builds}
}
/// Staged BEFORE transfer/IO. Fields and every failed SDK/read/repository/pin
/// owner remain in this original campaign; no retry/Drop cleanup is supplied.
pub(crate) struct ManagedCanonicalAcceptance{sdk:Arc<CompilerSdkClient>,snapshot_clock:FinalizationBudget,cancelled:Arc<AtomicBool>,phase:CanonicalState,
    acceptor:Option<ManagedCheckpointAcceptance>,writer:Option<Journal>,policy:Option<RetainedCanonicalPolicy>,
    input:Option<RetainedRunnableBuildInput>,local_input:Option<QualifiedRunnableBuildInput>,work:WorkDirectory,
    gate_clock:Option<FinalizationBudget>,intent_clock:Option<FinalizationBudget>,cleanup:Option<(FinalizationBudget,Arc<AtomicBool>)>,
    builds:Vec<BuildCell>,evidence:Option<Arc<CanonicalGateEvidence>>,previous:Option<Arc<CanonicalGateEvidence>>,permit:Option<PublicationPermit>,
    integration:Option<IntegrationReceipt>,writer_released:Option<Arc<AtomicBool>>,release:Option<repository::UnpublishedRelease>,attempted:bool,known:bool,error:Option<anyhow::Error>,observed:Option<VcsIntentReceipt>,reconciliation_error:Option<anyhow::Error>}
impl ManagedCanonicalAcceptance{
    pub(crate) fn staged(sdk:Arc<CompilerSdkClient>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,previous:Option<Arc<CanonicalGateEvidence>>)->Self{
        Self{sdk,snapshot_clock:original,cancelled,phase:CanonicalState::Captured,acceptor:None,writer:None,policy:None,input:None,local_input:None,work:WorkDirectory::staged(),gate_clock:None,intent_clock:None,cleanup:None,
            builds:Vec::new(),evidence:None,previous,permit:Some(PublicationPermit),integration:None,writer_released:None,release:None,attempted:false,known:false,error:None,observed:None,reconciliation_error:None}}
    pub(crate) fn run_from_ingress(&mut self,control:&crate::managed_machine_ingress::ManagedIngressControl,key:&str)->Result<()> {
        ensure!(!self.attempted,"original canonical campaign cannot replay");self.attempted=true;
        let result=(||->Result<()>{time(&self.snapshot_clock,&self.cancelled)?;self.sdk.current(&self.snapshot_clock)?;self.sdk.require_ordinary_execution()?;
            control.take_joined_for_acceptance_retaining(key,&mut self.acceptor,&self.snapshot_clock)?;self.run_inner()})();
        if let Err(e)=result{self.error=Some(e);self.phase=if self.permit.is_some(){CanonicalState::RetainedFailure}else{CanonicalState::Recovery};
            // This closes ONLY an original unpublished writer lock. It never
            // retires SDK/task readers, discards private work or claims a drain.
            if self.permit.is_some(){self.cleanup_unpublished()?;}else if let Some(original)=self.intent_clock.as_ref().cloned(){
                // Connected observation of the SAME attempted intent under its
                // SAME still-live clock. No repeat publisher/gates/new deadline,
                // no successful-source ACK from historical Published DATA.
                if original.remaining().is_ok(){if let Err(error)=self.observe_only(&original){self.reconciliation_error=Some(error);}}
            }anyhow::bail!("canonical campaign failed; retain all original owners")}
        Ok(())
    }
    fn input(&self)->Result<&BuildInputView>{if let Some(actual)=&self.input{return actual.input()}self.local_input.as_ref().map(QualifiedRunnableBuildInput::input).context("actual qualified candidate input absent")}
    fn run_inner(&mut self)->Result<()> {
        self.phase=CanonicalState::Snapshot;let a=self.acceptor.as_mut().context("original Joined acceptor absent")?;
        a.capture_acceptance_writer_retaining(&mut self.writer,&self.snapshot_clock,&self.cancelled)?;
        a.prepare_candidate_retaining(self.writer.as_ref().unwrap(),self.snapshot_clock.clone(),self.cancelled.clone())?;
        self.writer_released=Some(a.repository.as_ref().unwrap().writer_released.clone());
        self.policy=Some(RetainedCanonicalPolicy::staged());self.policy.as_mut().unwrap().capture(a,self.writer.as_ref().unwrap(),&self.snapshot_clock,&self.cancelled)?;
        let candidate=a.candidate.as_ref().unwrap().clone();let binding=a.binding.as_ref().unwrap().clone();
        let mut limits=InputBudget::default();limits.max_elapsed_ms=u64::try_from(self.snapshot_clock.remaining()?.as_millis().min(30_000))?;
        if let Some(actual)=&a.backed_candidate{let inputs=RetainedRunnableInputs::Overlay(actual.acknowledgment()?.clone());
            RetainedRunnableBuildInput::capture_retaining(&mut self.input,a.store.clone(),inputs,binding.projection_policy.clone(),limits,Arc::new(Fence{lease:a.lease.clone(),original:self.snapshot_clock.clone(),cancelled:self.cancelled.clone()}))?;
        }else{self.local_input=Some(rudder_engine::workspace::runnable_build_input_view(&a.store,&candidate.runnable,&binding.projection_policy,&limits)?);}
        self.work.capture(&a.location.store().binding().directory,&self.snapshot_clock,&self.cancelled)?;time(&self.snapshot_clock,&self.cancelled)?;
        self.phase=CanonicalState::Gates;self.gate_clock=Some(FinalizationBudget::begin(Duration::from_secs(30))?);let gate=self.gate_clock.as_ref().unwrap().clone();
        self.policy.as_ref().unwrap().snapshot.as_ref().unwrap().assert_current(self.writer.as_ref().unwrap(),&gate,&self.cancelled)?;
        self.acceptor.as_ref().unwrap().lease.wait_shared_current(&gate,&self.cancelled)?;
        let profiles=self.policy.as_ref().unwrap().profiles.clone();for profile in profiles{time(&gate,&self.cancelled)?;
            self.builds.push(BuildCell{pending:None,ready:None,terminal:None});let position=self.builds.len()-1;let input=self.input()?.clone();let store=self.acceptor.as_ref().unwrap().store.clone();
            let previous=self.previous.as_ref().and_then(|p|p.builds.iter().find_map(|b|b.outcome().ok().filter(|o|o.source_root()==input.source_root())));
            self.sdk.build_selected_source_retaining(&mut self.builds[position].pending,&input,&profile,&store,&self.work.path,gate.clone(),self.cancelled.clone(),previous)?;
            self.builds[position].ready=Some(Arc::new(self.builds[position].pending.take().context("actual SDK original build carrier absent")?));
            self.builds[position].ready.as_ref().unwrap().outcome()?.current(&gate)?;time(&gate,&self.cancelled)?;
        }
        self.capture_evidence(&gate)?;self.phase=CanonicalState::Intent;
        self.intent_clock=Some(FinalizationBudget::begin(Duration::from_secs(30))?);let intent=self.intent_clock.as_ref().unwrap().clone();
        let a=self.acceptor.as_mut().unwrap();let evidence=self.evidence.as_ref().unwrap();evidence.current(a,self.writer.as_ref().unwrap(),&intent,&self.cancelled)?;
        self.permit.take().context("original publication permit already spent")?;
        let progress=a.prepare_acceptance_retaining(self.writer.as_mut().unwrap(),evidence.data.clone(),evidence.snapshot.versions_data().clone(),intent.clone(),self.cancelled.clone())?;
        ensure!(matches!(progress,ManagedAcceptanceProgress::Prepared{..}),"canonical intent preparation requires original known Prepared result");self.phase=CanonicalState::Publishing;
        let progress=a.publish_canonical_once(self.writer.as_mut().unwrap(),evidence)?;let ManagedAcceptanceProgress::Published(receipt)=progress else{anyhow::bail!("canonical publication unknown; keep original campaign")};
        self.integration=Some(receipt);
        // Resource cleanup is a separate original operation. It cannot renew
        // gate/publication ACKs. Whole Prepared/SDK/checkpoint custody remains.
        self.cleanup=Some((FinalizationBudget::begin(Duration::from_secs(30))?,Arc::new(AtomicBool::new(false))));let(cleanup,_)=self.cleanup.as_ref().unwrap();
        a.settle_published_task(self.writer.as_ref().unwrap(),cleanup)?;cleanup.remaining()?;self.phase=CanonicalState::Published;self.known=true;Ok(())
    }
    fn capture_evidence(&mut self,original:&FinalizationBudget)->Result<()> {
        let a=self.acceptor.as_ref().unwrap();let policy=self.policy.as_ref().unwrap();ensure!(policy.ready,"original canonical policy ACK absent");
        let candidate=a.candidate.as_ref().unwrap().clone();let binding=a.binding.as_ref().unwrap().clone();let mut refs=Vec::new();let mut checks=Vec::new();let mut builds=Vec::new();
        for cell in &self.builds{let build=cell.ready.as_ref().context("original SDK build not acknowledged")?.clone();let outcome=build.outcome()?;outcome.current(original)?;
            ensure!(Arc::ptr_eq(outcome.qualified_owner(),&self.sdk)&&outcome.source_root()==&candidate.source&&outcome.checkpoint()==&candidate.runnable&&outcome.view_root()==&candidate.directory&&outcome.input_topology()==Some(&candidate.hardlinks),"compiler result not exact candidate");
            ensure!(!outcome.action_results().is_empty(),"actual compiler result references absent");for r in outcome.action_results(){r.validate()?;ensure!(r.kind==ObjectKind::ActionResult,"compiler result kind differs");refs.push(r.clone());}
            checks.push(serde_json::json!({"sdk_environment":outcome.sdk_environment(),"action_key":outcome.action_key(),"toolchains":outcome.toolchains(),"checks":outcome.checks(),"warm_reuse":outcome.warm_reuse(),"artifacts":outcome.artifacts()?}));builds.push(build);
        }refs.push(policy.record.as_ref().unwrap().clone());refs.sort();refs.dedup();let data=IntegrationEvidence{tested_head:binding.baseline.commit.0.clone(),tested_source_root:candidate.source.clone(),tested_environment_root:candidate.environment.clone(),action_results:refs,
            checks:serde_json::json!({"schema":"native-canonical-sdk-gates-v1","compiler_only":true,"policy_selection":policy.record,"profiles":checks,"buck2_frontend_executed":false,"cargo_executed":false,"reapi_cache_claim":false})};
        let evidence=Arc::new(CanonicalGateEvidence{sdk:self.sdk.clone(),builds,snapshot:policy.snapshot.as_ref().unwrap().clone(),assignment:a.completion.assignment().clone(),candidate,binding,policy_record:policy.record.as_ref().unwrap().clone(),data});
        evidence.current(a,self.writer.as_ref().unwrap(),original,&self.cancelled)?;self.evidence=Some(evidence);Ok(())
    }
    fn cleanup_unpublished(&mut self)->Result<()> {
        if self.cleanup.is_none(){self.cleanup=Some((FinalizationBudget::begin(Duration::from_secs(30))?,Arc::new(AtomicBool::new(false))));}
        let(original,cancel)=self.cleanup.as_ref().unwrap();original.remaining()?;
        let Some(a)=&mut self.acceptor else{return Ok(())};
        repository::release_unpublished_retaining(&mut self.release,a,self.permit.as_ref().context("original publication permit spent")?,original,cancel)
    }
    /// Called ONLY after the controller has retained the actual outer/TLS join.
    pub(crate) fn offer_after_join(&mut self,control:&crate::managed_machine_ingress::ManagedIngressControl,key:&str)->Result<bool>{
        ensure!(self.known&&self.phase==CanonicalState::Published,"canonical original publication/cleanup ACK absent");let(original,_)=self.cleanup.as_ref().unwrap();original.remaining()?;
        control.offer_published_acceptance_retaining(key,&mut self.acceptor,original)?;original.remaining()?;Ok(true)}
    pub(crate) fn observe_only(&mut self,original:&FinalizationBudget)->Result<()> {
        original.remaining()?;let a=self.acceptor.as_mut().context("original acceptor already transferred/absent")?;
        ensure!(a.intent.is_some(),"original intent ACK absent; unknown creation cannot be reconstructed");
        a.lease.wait_shared_current(original,&AtomicBool::new(false))?;
        let _=a.reconcile_only(self.writer.as_mut().context("original writer absent")?)?;
        self.observed=a.intent.clone();original.remaining()?;Ok(())
    }
    /// Only after the actual outer build/TLS join. Successful publications
    /// and separately acknowledged terminal HELPER failures can settle their
    /// own temporary resources. Failed carriers never enter gate evidence.
    /// Unknown/late/panicked carriers remain in pending and stay charged.
    pub(crate) fn sdk_jobs_for_settlement_after_join(&mut self)->Vec<Arc<RetainedCompilerSdkBuild>>{
        for cell in &mut self.builds{if cell.terminal.is_none()&&cell.pending.as_ref().is_some_and(|p|p.terminal_failure().is_ok()){
            cell.terminal=Some(Arc::new(cell.pending.take().unwrap()));}}
        self.builds.iter().flat_map(|cell|cell.ready.iter().chain(cell.terminal.iter()).cloned()).collect()
    }
    pub(crate) fn evidence(&self)->Option<Arc<CanonicalGateEvidence>>{self.evidence.clone()}
    pub(crate) fn state_data(&self)->CanonicalState{self.phase}
    pub(crate) fn may_release_serial_slot(&self)->bool{self.known&&self.writer_released.as_ref().is_some_and(|actual|actual.load(Ordering::Acquire))||self.permit.is_some()&&(self.acceptor.is_none()||self.release.as_ref().is_some_and(repository::UnpublishedRelease::acknowledged))}
    pub(crate) fn failure_data(&self)->Option<String>{self.error.as_ref().map(|e|format!("{e:#}"))}
}

struct CanonicalPublisher<'a>{inner:CheckedJjPublisher<'a>,lease:&'a EngineSourceLease,evidence:&'a CanonicalGateEvidence,original:&'a FinalizationBudget,cancel:&'a AtomicBool}
impl CanonicalPublisher<'_>{fn current(&self)->Result<()>{time(self.original,self.cancel)?;self.lease.wait_shared_current(self.original,self.cancel)?;
    self.inner.repository.check_current()?;self.evidence.execution_current(self.original,self.cancel)}}
impl VcsPublisher for CanonicalPublisher<'_>{
    fn inspect(&mut self,prepared:&PreparedVcsOperation)->Result<VcsPublicationObservation>{self.current()?;let result=self.inner.inspect(prepared);self.current()?;result}
    fn publish(&mut self,prepared:&PreparedVcsOperation)->Result<VcsPublicationObservation>{self.current()?;let result=self.inner.publish(prepared);self.current()?;result}
}
impl ManagedCheckpointAcceptance{
    /// Same original intent state machine as the legacy API, with the actual
    /// non-Serde SDK owner checked at BOTH real jj callback boundaries. Final
    /// Journal head/topic comparison remains in its existing SQL transaction.
    fn publish_canonical_once(&mut self,journal:&mut Journal,evidence:&CanonicalGateEvidence)->Result<ManagedAcceptanceProgress>{
        self.intent_time()?;self.lease.check_shared_current()?;self.check(journal)?;evidence.current(self,journal,&self.intent_work.as_ref().context("original canonical intent clock absent")?.original,
            &self.intent_work.as_ref().unwrap().cancelled)?;
        ensure!(!self.publish_attempted&&self.intent.as_ref().is_some_and(|r|r.state==VcsIntentState::Prepared),"canonical publication cannot replay");
        self.publish_attempted=true;let work=self.intent_work.as_ref().unwrap();
        let mut publisher=CanonicalPublisher{inner:CheckedJjPublisher{repository:self.repository.as_ref().unwrap(),store:&self.store},lease:&self.lease,evidence,original:&work.original,cancel:&work.cancelled};
        self.intent=Some(journal.publish_vcs_intent(self.lease.owner(),&self.intent.as_ref().unwrap().intent_id,&mut publisher,now_ms()?)?);
        self.repository.as_ref().unwrap().check_current()?;self.intent_time()?;
        self.publication_acknowledged=self.intent.as_ref().is_some_and(|r|r.state==VcsIntentState::Published);self.progress()
    }
}

struct WorkDirectory{path:PathBuf,parent:Option<File>,child:Option<File>,attempted:bool,ready:bool}
impl WorkDirectory{fn staged()->Self{Self{path:PathBuf::new(),parent:None,child:None,attempted:false,ready:false}}
    fn capture(&mut self,parent:&std::path::Path,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()>{ensure!(!self.attempted,"canonical work namespace cannot retry");self.attempted=true;time(original,cancel)?;
        ensure!(parent.starts_with("/workspace")&&parent.canonicalize()?==parent,"compiler work must be actual prepared /workspace namespace");
        self.parent=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(parent)?);let p=self.parent.as_ref().unwrap().metadata()?;
        ensure!(p.uid()==1500&&p.gid()==1500&&p.mode()&0o7777==0o700,"compiler work parent not original Native private owner");let name=format!("canonical-sdk-{}",uuid::Uuid::new_v4());self.path=parent.join(&name);let name=CString::new(name)?;
        ensure!(unsafe{libc::mkdirat(self.parent.as_ref().unwrap().as_raw_fd(),name.as_ptr(),0o700)}==0,"canonical original work mkdir outcome failed; retain namespace intent");
        let raw=unsafe{libc::openat(self.parent.as_ref().unwrap().as_raw_fd(),name.as_ptr(),libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_RDONLY)};ensure!(raw>=0,"canonical created work open failed");self.child=Some(unsafe{File::from_raw_fd(raw)});
        let c=self.child.as_ref().unwrap().metadata()?;let named=fs::symlink_metadata(&self.path)?;let n=fs::symlink_metadata(parent)?;
        ensure!(c.is_dir()&&c.uid()==1500&&c.gid()==1500&&c.mode()&0o7777==0o700&&(c.dev(),c.ino())==(named.dev(),named.ino())&&(p.dev(),p.ino(),p.mode(),p.uid(),p.gid())==(n.dev(),n.ino(),n.mode(),n.uid(),n.gid()),"canonical work custody changed");
        self.child.as_ref().unwrap().sync_all()?;self.parent.as_ref().unwrap().sync_all()?;time(original,cancel)?;self.ready=true;Ok(())}}
