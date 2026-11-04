//! Actual canonical completion consumer for SDK job resources. This shares
//! the existing worker bound; it never reopens source/gate/intent clocks.
use super::*;
use rudder_native::root_storage_bootstrap::runtime_image::{RetainedCompilerSdkBuild,CompilerSdkJobDisposal,CompilerSdkJobSettled};
struct Job {build:Arc<RetainedCompilerSdkBuild>,disposal:Option<CompilerSdkJobDisposal>,settled:Option<CompilerSdkJobSettled>,closed:bool,finalize_attempted:bool,error:Option<anyhow::Error>}
struct Owner {jobs:Vec<Job>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,build_joined:WorkerJoined,
    worker:Option<OwnedWorkerJoin>,joined:Option<WorkerJoined>,charged:bool,panic:Option<Box<dyn Any+Send>>,error:Option<anyhow::Error>}
pub(super) struct Cleanup {owner:Mutex<Owner>,worker_owned:AtomicBool}
impl Cleanup {
    /// Known lack of pool capacity has no effect and captures no operation.
    /// A captured cleanup budget is installed before its original spawn.
    pub(super) fn capture_retaining(out:&mut Option<Arc<Self>>,shared:&Arc<Shared>,builds:Vec<Arc<RetainedCompilerSdkBuild>>,build_joined:&WorkerJoined)->Result<bool>{
        ensure!(out.is_none(),"original canonical SDK cleanup already captured");if builds.is_empty(){return Ok(true)}
        if !reserve(&shared.active){return Ok(false)}
        let original=match FinalizationBudget::begin(Duration::from_secs(30)){Ok(v)=>v,Err(e)=>{shared.active.fetch_sub(1,Ordering::AcqRel);return Err(e)}};
        *out=Some(Arc::new(Self{owner:Mutex::new(Owner{jobs:builds.into_iter().map(|build|Job{build,disposal:None,settled:None,closed:false,finalize_attempted:false,error:None}).collect(),original,cancelled:Arc::new(AtomicBool::new(false)),build_joined:build_joined.clone(),
            worker:None,joined:None,charged:true,panic:None,error:None}),worker_owned:AtomicBool::new(false)}));let actual=out.as_ref().unwrap().clone();
        let worker=actual.clone();let spawned=thread::Builder::new().name("rudder-sdk-job-cleanup".into()).spawn(move||{
            while !worker.worker_owned.load(Ordering::Acquire){thread::yield_now();}
            let mut owner=worker.owner.lock().unwrap_or_else(|p|p.into_inner());
            let result=catch_unwind(AssertUnwindSafe(||{
                let original=owner.original.clone();let cancelled=owner.cancelled.clone();let joined=owner.build_joined.clone();
                for job in &mut owner.jobs {let result=if job.build.outcome().is_ok(){
                    job.build.begin_job_disposal_retaining(&mut job.disposal,&joined,original.clone(),cancelled.clone())
                }else{job.build.begin_terminal_failure_disposal_retaining(&mut job.disposal,&joined,original.clone(),cancelled.clone())};
                    match result{Ok(())=>job.closed=true,Err(error)=>job.error=Some(error)}}
            }));if let Err(panic)=result{owner.panic=Some(panic);}
        });
        let mut owner=actual.owner.lock().unwrap_or_else(|p|p.into_inner());match spawned{
            Ok(worker)=>{owner.worker=Some(OwnedWorkerJoin::new(worker));actual.worker_owned.store(true,Ordering::Release);Ok(true)},
            Err(error)=>{owner.error=Some(error.into());owner.charged=false;shared.active.fetch_sub(1,Ordering::AcqRel);anyhow::bail!("SDK cleanup thread creation failed; original cleanup/results retained")}
        }
    }
    pub(super) fn poll_off_app(&self,shared:&Arc<Shared>){let mut owner=match self.owner.try_lock(){Ok(owner)=>owner,Err(_)=>return};
        if owner.joined.is_none(){let Owner{worker,joined,charged,..}=&mut *owner;if let Some(worker)=worker{match record_original_join(worker,joined,charged,&shared.active){Ok(true)=>{},Ok(false)=>return,Err(error)=>{owner.error=Some(error);return}}}}
        let Some(joined)=owner.joined.as_ref().cloned()else{return};
        for job in &mut owner.jobs {if !job.closed||job.finalize_attempted{continue}job.finalize_attempted=true;
            let Some(disposal)=&job.disposal else{continue};match disposal.settle_after_join_retaining(&mut job.settled,&joined){
                Ok(())=>{if !job.settled.as_ref().is_some_and(|ack|ack.matches_original(&job.build)){job.error=Some(anyhow::anyhow!("SDK actual job settlement receipt differs"));}},
                Err(error)=>job.error=Some(error)
            }
        }
    }
}

impl Cleanup {
    pub(super) fn observe_original_producer_worker(&self)->Result<Option<crate::managed_machine_entry::census::WorkerCensus>> {
        use crate::managed_machine_entry::census::{self,WorkerCensus};
        let worker={let Some(held)=census::borrow(&self.owner)?else{return Ok(None)};held.worker.clone()};
        Ok(Some(WorkerCensus::observe(worker.as_ref())?))
    }
}