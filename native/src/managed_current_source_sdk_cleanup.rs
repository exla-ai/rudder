//! Actual success/terminal-helper-failure SDK disposal adapter. Neither a callback,
//! finished flag nor an ordinary thread join substitutes for its opaque ACKs.
use super::*;
use rudder_engine::execution::handoff::WorkerJoined;
use rudder_native::root_storage_bootstrap::runtime_image::{CompilerSdkJobDisposal,CompilerSdkJobSettled};
struct Job {disposal:Option<CompilerSdkJobDisposal>,settled:Option<CompilerSdkJobSettled>}
pub(crate) struct CurrentSourceSdkCleanup {campaign:Arc<Mutex<CurrentSourceSdkCampaign>>,app_thread:std::thread::ThreadId,build_joined:WorkerJoined,
    original:FinalizationBudget,cancelled:Arc<AtomicBool>,jobs:Vec<Job>,cleanup_thread:Option<std::thread::ThreadId>,cleanup_joined:Option<WorkerJoined>,
    attempted:bool,finalize_attempted:bool,io_returned:bool,settled:bool,error:Option<anyhow::Error>}
impl CurrentSourceSdkCleanup {
    /// Retain BEFORE queue/spawn. The token itself is genuine; the SDK API
    /// additionally matches its original ThreadId to EACH actual build job.
    pub(crate) fn capture_retaining(out:&mut Option<Self>,campaign:Arc<Mutex<CurrentSourceSdkCampaign>>,build_joined:WorkerJoined,
        original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original current-source SDK cleanup occupied");
        // Pure/nonblocking metadata admission before queue; never wait on App.
        let app_thread=campaign.try_lock().map_err(|_|anyhow::anyhow!("original current-source campaign busy/poisoned before cleanup capture"))?.control.shared.app_thread;
        *out=Some(Self{campaign,app_thread,build_joined,original,cancelled,jobs:Vec::new(),cleanup_thread:None,cleanup_joined:None,attempted:false,finalize_attempted:false,io_returned:false,settled:false,error:None});Ok(())
    }
    fn time(&self)->Result<()>{time(&self.original,&self.cancelled)}
    /// On a DISTINCT retained cleanup worker AFTER actual build worker/TLS join.
    /// The same cleanup30 is passed to all jobs; it cannot renew execution or
    /// manufacture a successful outcome from a terminal helper failure.
    pub(crate) fn run_off_app_retaining(&mut self)->Result<()> {
        ensure!(std::thread::current().id()!=self.app_thread,"SDK cleanup requires actual off-App worker");
        ensure!(!self.attempted,"original SDK cleanup cannot replay");self.attempted=true;self.cleanup_thread=Some(std::thread::current().id());
        let result=self.close();if let Err(error)=result{self.error=Some(error);anyhow::bail!("current-source SDK cleanup failed; every original owner retained")};Ok(())
    }
    fn close(&mut self)->Result<()> {
        self.time()?;let campaign=self.campaign.clone();let held=lock_campaign(&campaign,&self.original,&self.cancelled)?;
        ensure!(std::thread::current().id()!=held.control.shared.app_thread,"SDK disposal requires actual off-App cleanup worker");
        ensure!(!held.builds.is_empty(),"original SDK campaign has no actual build jobs");
        // Source/head failure after an actual SDK result must not prevent
        // private-resource cleanup. Only the genuine successful publication
        // OR distinct timely terminal-HELPER-failure owner selects a path.
        // Arbitrary Err, live/late/unknown and absent carriers stay charged.
        for build in &held.builds{let actual=build.original.as_ref().context("original SDK effect absent/unknown")?;
            ensure!(actual.outcome().is_ok()||actual.terminal_failure().is_ok(),"SDK job has no original successful or terminal-helper-failure receipt; retain charge");}
        self.jobs=(0..held.builds.len()).map(|_|Job{disposal:None,settled:None}).collect();
        for (build,job) in held.builds.iter().zip(&mut self.jobs) {let build=build.original.as_ref().context("original SDK carrier absent")?;
            if build.outcome().is_ok(){build.begin_job_disposal_retaining(&mut job.disposal,&self.build_joined,self.original.clone(),self.cancelled.clone())?;}
            else{build.terminal_failure()?;build.begin_terminal_failure_disposal_retaining(&mut job.disposal,&self.build_joined,self.original.clone(),self.cancelled.clone())?;}}
        self.time()?;self.io_returned=true;Ok(())
    }
    /// On the existing join observer ONLY after the cleanup worker's actual
    /// original JoinHandle::join incl TLS. Every SDK finalizer rechecks exact
    /// cleanup ThreadId/Root ACK/full close under the SAME stored cleanup clock.
    pub(crate) fn settle_after_cleanup_join_retaining(&mut self,actual_cleanup_joined:WorkerJoined)->Result<()> {
        ensure!(std::thread::current().id()!=self.app_thread,"SDK finalization requires actual off-App observer");
        self.time()?;ensure!(self.io_returned&&!self.settled&&!self.finalize_attempted&&self.error.is_none(),"original SDK cleanup IO failed/unknown/already finalized");self.finalize_attempted=true;
        self.cleanup_joined=Some(actual_cleanup_joined);require_join(self.cleanup_joined.as_ref().unwrap(),self.cleanup_thread.context("original cleanup worker absent")?)?;
        let campaign=self.campaign.clone();let held=lock_campaign(&campaign,&self.original,&self.cancelled)?;
        ensure!(std::thread::current().id()!=held.control.shared.app_thread&&held.builds.len()==self.jobs.len(),"SDK finalization is off App and exact original jobs only");
        for (build,job) in held.builds.iter().zip(&mut self.jobs){let build=build.original.as_ref().context("original SDK carrier absent")?;
            job.disposal.as_ref().context("original SDK resource closure absent")?.settle_after_join_retaining(&mut job.settled,self.cleanup_joined.as_ref().unwrap())?;
            let proof=job.settled.as_ref().context("actual SDK settlement ACK absent")?;ensure!(proof.matches_original(build),"SDK settlement belongs to another original job");proof.acknowledged()?;}
        self.time()?;self.settled=true;Ok(())
    }
    /// Purely historical DATA. Actual opaque proofs remain in this same owner;
    /// pins/SDK/source/shared runtime resources have not been released here.
    pub(crate) fn settled_data(&self)->bool{self.settled}
    pub(crate) fn failure_data(&self)->Option<String>{self.error.as_ref().map(|e|format!("{e:#}"))}
}
fn lock_campaign<'a>(value:&'a Mutex<CurrentSourceSdkCampaign>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<std::sync::MutexGuard<'a,CurrentSourceSdkCampaign>>{loop{time(original,cancelled)?;match value.try_lock(){Ok(held)=>return Ok(held),Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(1))?),Err(_)=>anyhow::bail!("current SDK original campaign poisoned")}}}
fn require_join(proof:&WorkerJoined,original:std::thread::ThreadId)->Result<()>{ensure!(proof.original_thread_id_data()==original,"cleanup proof is not actual original worker/TLS join");Ok(())}
#[cfg(test)]#[path="managed_current_source_sdk_cleanup_tests.rs"]mod tests;
