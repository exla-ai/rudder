//! Explicit per-job temporary-resource disposal. Immutable execution/result,
//! source/artifact pins and the SAME SDK/floor client remain owned and current.
use super::*;
use rudder_engine::execution::{handoff::WorkerJoined,runtime_image::RuntimeDisposalRead};
use super::super::super::sdk_job_protocol::{CloseRequest,CloseAck,FailureCloseRequest,FailureCloseAck};
#[derive(Clone,Copy)]enum DisposalSubject{Published,TerminalFailure}
enum JobCloseRequest{Published(CloseRequest),TerminalFailure(FailureCloseRequest)}
enum JobCloseAck{Published(CloseAck),TerminalFailure(FailureCloseAck)}
impl JobCloseRequest{fn bytes(&self)->Result<Vec<u8>>{match self{Self::Published(v)=>canonical_json(v),Self::TerminalFailure(v)=>canonical_json(v)}}
    fn parse_ack(&self,raw:&[u8])->Result<JobCloseAck>{match self{Self::Published(expected)=>{let ack:CloseAck=serde_json::from_slice(raw)?;ack.validate(expected)?;Ok(JobCloseAck::Published(ack))},Self::TerminalFailure(expected)=>{let ack:FailureCloseAck=serde_json::from_slice(raw)?;ack.validate(expected)?;Ok(JobCloseAck::TerminalFailure(ack))}}}}
impl JobCloseAck{fn helper(&self)->(u32,&str){match self{Self::Published(v)=>(v.original_helper_pid,&v.original_helper_start),Self::TerminalFailure(v)=>(v.original_helper_pid,&v.original_helper_start)}}}

pub(super) struct DisposalState {original:FinalizationBudget,cancelled:Arc<AtomicBool>,build_worker:WorkerJoined,cleanup_thread:std::thread::ThreadId,
    cleanup_worker:Option<WorkerJoined>,subject:DisposalSubject,attempted:bool,request:Option<JobCloseRequest>,bytes:Option<Vec<u8>>,cancel:Option<OwnedFd>,
    sent:bool,shutdown_attempted:bool,shutdown:bool,pending:Option<RuntimePacket>,ack:Option<JobCloseAck>,root_eof:bool,
    aliases_closed:bool,resources_closed:bool,settled:bool,error:Option<anyhow::Error>,panic:Option<Box<dyn std::any::Any+Send>>}
/// Actual original operation owner, staged before IO and retained on every
/// error/panic. It cannot be built from a serial, JSON, path or a bool.
pub struct CompilerSdkJobDisposal {original:FinalizationBudget,cancelled:Arc<AtomicBool>,build:Arc<Mutex<BuildState>>,state:Arc<Mutex<DisposalState>>,sdk:Arc<CompilerSdkClient>}
/// Per-job capacity ACK only. No shared FUSE/read/actor/runtime retirement,
/// execution/cacheability/source/lease grant or pin release is implied.
pub struct CompilerSdkJobSettled {build:Arc<Mutex<BuildState>>,state:Arc<Mutex<DisposalState>>}
pub(super) fn register_active_retaining(sdk:&Arc<CompilerSdkClient>,state:&Arc<Mutex<BuildState>>,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
    let mut actual=lock(state,original,cancel)?;let mut retained=lock(&sdk.builds,original,cancel)?;
    ensure!(!sdk.build_admission_closed.load(Ordering::Acquire),"SDK original build admission closed");
    ensure!(retained.len()<MAX_HELPERS&&!actual.registered,"SDK retained builds full/original already registered; actual job settlement required");
    job_protocol::time(original,cancel)?;retained.push(state.clone());actual.registered=true;Ok(())
}
fn lock<'a,T>(value:&'a Mutex<T>,original:&FinalizationBudget,cancel:&AtomicBool)->Result<std::sync::MutexGuard<'a,T>>{loop{job_protocol::time(original,cancel)?;match value.try_lock(){
    Ok(held)=>return Ok(held),Err(TryLockError::WouldBlock)=>std::thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(_)=>anyhow::bail!("SDK original disposal owner poisoned")}}}
fn require_original_worker(proof:&WorkerJoined,actual:std::thread::ThreadId)->Result<()>{ensure!(proof.original_thread_id_data()==actual,"SDK disposal belongs to another actual original worker");Ok(())}
// Pure registry bookkeeping. Authority/read/FD/clock checks remain in the
// opaque owner above this helper; Arc equality is never a Root grant.
fn remove_original_slot<T>(registry:&mut Vec<Arc<T>>,actual:&Arc<T>)->Result<()>{let index=registry.iter().position(|entry|Arc::ptr_eq(entry,actual)).context("SDK actual charged job absent")?;registry.remove(index);Ok(())}
impl RetainedCompilerSdkBuild {
    /// OFF App on a retained cleanup worker. original_worker must be the REAL
    /// already-joined thread that ran this synchronous build, including TLS.
    /// This closes resources only; finalize requires THIS cleanup worker join.
    pub fn begin_job_disposal_retaining(&self,out:&mut Option<CompilerSdkJobDisposal>,original_worker:&WorkerJoined,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original SDK job disposal already occupied");let outcome=self.outcome()?;
        ensure!(Arc::ptr_eq(&outcome.owner,&self.state),"SDK original result carrier differs");
        self.begin_disposal_subject_retaining(out,original_worker,original,cancelled,DisposalSubject::Published,outcome.sdk.clone())
    }
    /// Same actual build-thread/TLS join and private resource closure as the
    /// success path, but ONLY a genuine timely terminal HELPER-failure owner.
    /// No artifact/build/cache or canonical-success evidence is constructed.
    pub fn begin_terminal_failure_disposal_retaining(&self,out:&mut Option<CompilerSdkJobDisposal>,original_worker:&WorkerJoined,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original failed SDK disposal already occupied");let failure=self.terminal_failure()?;
        ensure!(failure.matches_owner(&self.state),"SDK original failed carrier differs");
        self.begin_disposal_subject_retaining(out,original_worker,original,cancelled,DisposalSubject::TerminalFailure,failure.qualified_owner().clone())
    }
    fn begin_disposal_subject_retaining(&self,out:&mut Option<CompilerSdkJobDisposal>,original_worker:&WorkerJoined,original:FinalizationBudget,cancelled:Arc<AtomicBool>,subject:DisposalSubject,sdk:Arc<CompilerSdkClient>)->Result<()> {
        let state=Arc::new(Mutex::new(DisposalState{original:original.clone(),cancelled:cancelled.clone(),build_worker:original_worker.clone(),cleanup_thread:std::thread::current().id(),cleanup_worker:None,subject,
            attempted:false,request:None,bytes:None,cancel:None,sent:false,shutdown_attempted:false,shutdown:false,pending:None,ack:None,root_eof:false,aliases_closed:false,resources_closed:false,settled:false,error:None,panic:None}));
        *out=Some(CompilerSdkJobDisposal{original:original.clone(),cancelled:cancelled.clone(),build:self.state.clone(),state,sdk});
        let owner=out.as_ref().unwrap();let result=catch_unwind(AssertUnwindSafe(||owner.close_resources()));
        match result{Ok(Ok(()))=>Ok(()),Ok(Err(error))=>{owner.state.lock().unwrap_or_else(|p|p.into_inner()).error=Some(error);anyhow::bail!("SDK job disposal failed; original closure/charge retained")},
            Err(panic)=>{owner.state.lock().unwrap_or_else(|p|p.into_inner()).panic=Some(panic);anyhow::bail!("SDK job disposal panicked; original closure/charge retained")}}
    }
}
fn subject_current(subject:DisposalSubject,build:&BuildState)->Result<()>{match subject{DisposalSubject::Published=>build.result_current(),DisposalSubject::TerminalFailure=>build.failure_current()}}
impl CompilerSdkJobDisposal {
    fn close_resources(&self)->Result<()> {
        let original=self.original.clone();let cancel=self.cancelled.clone();
        let mut d=lock(&self.state,&original,&cancel)?;ensure!(d.original.constraint()==original.constraint()&&Arc::ptr_eq(&d.cancelled,&cancel),"SDK cleanup clock/cancellation carrier changed");ensure!(!d.attempted,"original SDK resource disposal cannot replay");d.attempted=true;
        let mut build=lock(&self.build,&original,&cancel)?;
        ensure!(build.disposal.is_none(),"SDK job already has its actual original disposal owner");build.disposal=Some(self.state.clone());
        require_original_worker(&d.build_worker,build.caller_thread)?;ensure!(Arc::ptr_eq(&build.sdk,&self.sdk),"SDK disposal belongs to another actual client");
        ensure!(build.registered,"SDK original charged slot absent");
        self.sdk.current(&original)?;subject_current(d.subject,&build)?;job_protocol::time(&original,&cancel)?;
        // Every ordinary build executes. Preparation reuse never permits
        // skipping the original Root helper's disposal ACK and pipe EOF.
        d.request=Some(match d.subject{
            DisposalSubject::Published=>{let execution=build.execution.as_ref().context("SDK actual original Root execution ACK absent")?;
                ensure!(execution.ready&&build.dispatch_permit.is_none()&&build.sent&&build.published,"SDK original cold build effect/publication unknown");
                JobCloseRequest::Published(CloseRequest{version:1,kind:"compiler_sdk_job_close_v1".into(),binding:execution.binding.clone(),artifact_result:build.result.as_ref().unwrap().clone(),cleanup_constraint:original.constraint()})},
            DisposalSubject::TerminalFailure=>{let(binding,helper)=build.terminal_data()?;
                JobCloseRequest::TerminalFailure(FailureCloseRequest{version:1,kind:"compiler_sdk_failed_job_close_v1".into(),binding:binding.clone(),helper:helper.clone(),cleanup_constraint:original.constraint()})}
        });
        d.bytes=Some(d.request.as_ref().unwrap().bytes()?);let limit=match d.subject{DisposalSubject::Published=>4096,DisposalSubject::TerminalFailure=>16*1024};ensure!(d.bytes.as_ref().unwrap().len()<=limit,"SDK original close frame bound");d.cancel=Some(cancel_fd()?);
        // Only private BuildState-owned aliases are exposed by this API. Its
        // mutex excludes current calls; the actual build/TLS owner is joined.
        // Immutable result accessors expose no job FD/path/pipe API.
        build.remote_reply.take();build.source.take();build.output.take();build.cancel.take();d.aliases_closed=true;
        loop {
            if cancel.load(Ordering::Acquire)||original.remaining().is_err(){if let Some(fd)=&d.cancel{let _=signal_cancel(fd);}job_protocol::time(&original,&cancel)?;}
            self.sdk.current(&original)?;let endpoint=build.reply.as_ref().context("SDK original private reply absent")?;
            if !d.sent{if !endpoint.send_data(d.bytes.as_ref().unwrap(),&[d.cancel.as_ref().unwrap()])?{std::thread::sleep(original.phase_limit(Duration::from_millis(1))?);continue;}d.sent=true;}
            if !d.shutdown{ensure!(!d.shutdown_attempted,"SDK original write shutdown uncertain");d.shutdown_attempted=true;endpoint.shutdown_disposal_write()?;d.shutdown=true;}
            if d.ack.is_none(){match endpoint.receive_disposal_retaining(&mut d.pending)?{
                RuntimeDisposalRead::Pending=>{std::thread::sleep(original.phase_limit(Duration::from_millis(1))?);continue;},RuntimeDisposalRead::ReadClosed{..}=>anyhow::bail!("SDK Root closed without original disposal ACK"),RuntimeDisposalRead::Packet=>{}}
                let packet=d.pending.as_ref().unwrap();ensure!(packet.credentials.pid==self.sdk.client.service.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0&&packet.fds.is_empty(),"SDK disposal ACK is not actual original Root service");
                d.ack=Some(d.request.as_ref().unwrap().parse_ack(&packet.bytes)?);
                let(pid,start)=d.ack.as_ref().unwrap().helper();let expected=match d.subject{DisposalSubject::Published=>{let e=build.execution.as_ref().unwrap();(e.helper_pid,e.helper_start.as_str())},DisposalSubject::TerminalFailure=>{let(_,h)=build.terminal_data()?;(h.original_helper_pid,h.original_helper_start.as_str())}};
                ensure!((pid,start)==expected,"SDK disposal closed another original helper");
                job_protocol::time(&original,&cancel)?;d.pending.take();
            }
            match endpoint.receive_disposal_retaining(&mut d.pending)?{
                RuntimeDisposalRead::Pending=>{},RuntimeDisposalRead::Packet=>anyhow::bail!("SDK Root disposal sent extra data/descriptors"),
                RuntimeDisposalRead::ReadClosed{peer_fully_closed:true}=>{d.root_eof=true;break},RuntimeDisposalRead::ReadClosed{peer_fully_closed:false}=>{}}
            std::thread::sleep(original.phase_limit(Duration::from_millis(1))?);
        }
        job_protocol::time(&original,&cancel)?;self.sdk.current(&original)?;subject_current(d.subject,&build)?;
        build.reply.take();build.parent.take();build.work_fd.take();d.cancel.take();
        ensure!(build.reply.is_none()&&build.remote_reply.is_none()&&build.source.is_none()&&build.output.is_none()&&build.cancel.is_none()&&build.parent.is_none()&&build.work_fd.is_none(),"SDK native job descriptor alias remains");
        ensure!(d.root_eof&&d.ack.is_some(),"SDK original Root resource/EOF proof absent");
        job_protocol::time(&original,&cancel)?;d.resources_closed=true;Ok(())
    }
    /// OFF App after the ACTUAL cleanup worker/TLS join. Same stored cleanup
    /// clock/cancel only. The final registry edit is pure under both locks,
    /// following the last clock/custody/pin checks. No post-removal IO exists.
    pub fn settle_after_join_retaining(&self,out:&mut Option<CompilerSdkJobSettled>,cleanup_joined:&WorkerJoined)->Result<()> {
        ensure!(out.is_none(),"SDK original final settlement slot occupied");*out=Some(CompilerSdkJobSettled{build:self.build.clone(),state:self.state.clone()});
        let original=self.original.clone();let cancel=self.cancelled.clone();
        let mut d=lock(&self.state,&original,&cancel)?;ensure!(!d.settled&&d.resources_closed&&d.error.is_none()&&d.panic.is_none(),"SDK original resource closure unknown/failed");
        d.cleanup_worker=Some(cleanup_joined.clone());require_original_worker(cleanup_joined,d.cleanup_thread)?;
        let mut build=lock(&self.build,&original,&cancel)?;ensure!(build.disposal.as_ref().is_some_and(|owner|Arc::ptr_eq(owner,&self.state))&&build.registered,"SDK original active slot differs");
        self.sdk.current(&original)?;subject_current(d.subject,&build)?;
        let mut registry=lock(&self.sdk.builds,&original,&cancel)?;
        job_protocol::time(&original,&cancel)?;
        remove_original_slot(&mut registry,&self.build)?;build.registered=false;d.settled=true;Ok(())
    }
    pub fn failure_data(&self)->Result<Option<String>>{let d=self.state.try_lock().map_err(|_|anyhow::anyhow!("SDK disposal borrowed/poisoned"))?;Ok(d.error.as_ref().map(|e|format!("{e:#}")))}
}
#[cfg(test)]#[path="compiler_sdk_build_settlement_tests.rs"]mod tests;
impl CompilerSdkJobSettled {
    pub fn matches_original(&self,build:&RetainedCompilerSdkBuild)->bool{Arc::ptr_eq(&self.build,&build.state)}
    pub fn acknowledged(&self)->Result<()>{let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("SDK original settlement borrowed/poisoned"))?;ensure!(state.settled&&state.resources_closed&&state.cleanup_worker.is_some(),"SDK original actual settlement ACK absent");Ok(())}
}
