//! Completion-consumer hook on the existing ingress IO loop. Original task
//! preparation and canonical work share WORKERS. No App mutation or scheduler.
use super::*;
use crate::managed_checkpoint_accept::canonical::{ManagedCanonicalAcceptance,CanonicalState};
use std::sync::Weak;

#[path="managed_sdk_job_cleanup.rs"]mod sdk_cleanup;

struct State {sdk_cleanup:Option<Arc<sdk_cleanup::Cleanup>>,work:ManagedCanonicalAcceptance,worker:Option<OwnedWorkerJoin>,joined:Option<WorkerJoined>,charged:bool,
    offer_attempted:bool,offered:bool,error:Option<anyhow::Error>,observation_error:Option<String>,panic:Option<Box<dyn Any+Send>>}
pub(super) struct Campaign {state:Mutex<State>,worker_owned:AtomicBool,project:Weak<ProjectNode>}
fn reserve(active:&AtomicUsize)->bool{active.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|n.checked_add(1).filter(|next|*next<=WORKERS)).is_ok()}

pub(super) fn poll(shared:&Arc<Shared>) {
    let jobs:Vec<_>=shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).values().cloned().collect();
    for job in &jobs {
        let campaign=match job.carrier.try_lock(){Ok(held)=>held.canonical.clone(),Err(_)=>None};
        if let Some(campaign)=campaign {poll_original(shared,job,&campaign);}
    }
    let wanted=jobs.iter().any(|job|job.carrier.try_lock().is_ok_and(|held|held.canonical.is_none()&&held.joined.is_some()&&held.task.joined.is_some()));
    if !wanted{return;}
    if shared.config.data().compiler_sdk.is_none(){for job in &jobs{if let Ok(mut held)=job.carrier.try_lock(){if held.task.joined.is_some(){held.canonical_wait=Some("qualified_compiler_sdk_not_configured");}}}return;}
    // At most ONE effect-free actual-client lookup per ingress tick; retain the
    // original machine alias. No per-job filesystem/readiness polling loop.
    let mut actual=shared.canonical_sdk.lock().unwrap_or_else(|p|p.into_inner()).clone();
    if actual.is_none(){let observed=(||->Result<Option<Arc<rudder_native::root_storage_bootstrap::runtime_image::CompilerSdkClient>>>{
        let observation=FinalizationBudget::begin(Duration::from_millis(100))?;
        let Some(runtime)=shared.control.runtime_environment_client_off_app(&observation,&AtomicBool::new(false))? else{return Ok(None)};
        let sdk=runtime.compiler_sdk_client()?;observation.remaining()?;Ok(sdk)
    })();let Ok(Some(sdk))=observed else{return;};
        *shared.canonical_sdk.lock().unwrap_or_else(|p|p.into_inner())=Some(sdk.clone());actual=Some(sdk);}
    let sdk=actual.unwrap();
    for job in jobs {
        if shared.active.load(Ordering::Acquire)>=WORKERS {break;}
        let node={let held=match job.carrier.try_lock(){Ok(held)=>held,Err(_)=>continue};
            if held.canonical.is_some()||held.joined.is_none()||held.task.joined.is_none(){continue;}
            let Some(node)=&held.node else{continue;};node.clone()};
        if shared.canonical_projects.lock().unwrap_or_else(|p|p.into_inner()).contains_key(&node.alias){continue;}
        let original=match FinalizationBudget::begin(Duration::from_secs(30)){Ok(clock)=>clock,Err(_)=>continue};
        let previous=shared.canonical_previous.lock().unwrap_or_else(|p|p.into_inner()).get(&node.alias).cloned();
        let campaign=Arc::new(Campaign{state:Mutex::new(State{sdk_cleanup:None,work:ManagedCanonicalAcceptance::staged(sdk.clone(),original,Arc::new(AtomicBool::new(false)),previous),worker:None,joined:None,
            charged:false,offer_attempted:false,offered:false,error:None,observation_error:None,panic:None}),worker_owned:AtomicBool::new(false),project:Arc::downgrade(&node)});
        // Original cell and serial reservation are installed before thread
        // spawn. No failed/unknown campaign is replaced by a fresh clock.
        {let mut held=match job.carrier.try_lock(){Ok(held)=>held,Err(_)=>continue};if held.canonical.is_some(){continue;}
            let mut projects=shared.canonical_projects.lock().unwrap_or_else(|p|p.into_inner());if projects.contains_key(&node.alias){continue;}
            if !reserve(&shared.active){break;}
            campaign.state.lock().unwrap_or_else(|p|p.into_inner()).charged=true;held.canonical=Some(campaign.clone());held.canonical_wait=None;projects.insert(node.alias.clone(),campaign.clone());}
        let actual=campaign.clone();let ingress=Arc::downgrade(shared);let task=Arc::downgrade(&job);
        let started=thread::Builder::new().name("rudder-canonical-gates".into()).spawn(move||{
            while !actual.worker_owned.load(Ordering::Acquire){thread::yield_now();}
            let mut held=actual.state.lock().unwrap_or_else(|p|p.into_inner());
            // Carrier is outside catch. Unknown effects, failed SDK children,
            // original completion and panic payload stay in that same cell.
            let result=catch_unwind(AssertUnwindSafe(||->Result<()>{
                let shared=ingress.upgrade().context("original ingress owner absent")?;let job=task.upgrade().context("original task owner absent")?;
                let control=ManagedIngressControl{shared:shared.clone(),app_thread:shared.app_thread};
                held.work.run_from_ingress(&control,&job.command.command_key)
            }));
            match result{Ok(Ok(()))=>{},Ok(Err(e))=>held.error=Some(e),Err(p)=>held.panic=Some(p)}
        });
        let mut held=campaign.state.lock().unwrap_or_else(|p|p.into_inner());match started{
            Ok(worker)=>{held.worker=Some(OwnedWorkerJoin::new(worker));campaign.worker_owned.store(true,Ordering::Release);},
            Err(error)=>{held.error=Some(error.into());held.charged=false;shared.active.fetch_sub(1,Ordering::AcqRel);}
        }
    }
}

fn poll_original(shared:&Arc<Shared>,job:&Arc<Job>,campaign:&Arc<Campaign>) {
    let mut held=match campaign.state.try_lock(){Ok(held)=>held,Err(_)=>return};
    if held.joined.is_none(){let State{worker,joined,charged,..}=&mut *held;if let Some(worker)=worker{match record_original_join(worker,joined,charged,&shared.active){
        Ok(true)=>{},Ok(false)=>return,Err(e)=>{held.error=Some(e);return}
    }}}
    if held.worker.is_some()&&held.joined.is_none(){return;}
    if held.joined.is_some()&&!held.offer_attempted&&held.work.state_data()==CanonicalState::Published {
        held.offer_attempted=true;let control=ManagedIngressControl{shared:shared.clone(),app_thread:shared.app_thread};
        match held.work.offer_after_join(&control,&job.command.command_key){Ok(true)=>{held.offered=true;if let(Some(node),Some(evidence))=(campaign.project.upgrade(),held.work.evidence()){
            shared.canonical_previous.lock().unwrap_or_else(|p|p.into_inner()).insert(node.alias.clone(),evidence);}},Ok(false)=>{},Err(e)=>held.error=Some(e)}
    }
    // Resource settlement is independent of source publication/offer. It has
    // its own one-time original cleanup30 and original cleanup/TLS worker.
    if held.joined.is_some()&&held.sdk_cleanup.is_none(){let builds=held.work.sdk_jobs_for_settlement_after_join();if !builds.is_empty(){let joined=held.joined.as_ref().unwrap().clone();
        match sdk_cleanup::Cleanup::capture_retaining(&mut held.sdk_cleanup,shared,builds,&joined){Ok(_)=>{},Err(error)=>held.error=Some(error)}}}
    if let Some(cleanup)=&held.sdk_cleanup{cleanup.poll_off_app(shared);}
    // A published campaign retains serial ownership through the REAL
    // successor snapshot ACK and actual original writer unlock, not only its
    // offer, callback, finished message or outer worker join.
    if held.work.may_release_serial_slot()&&(held.worker.is_none()||held.joined.is_some()) {
        if let Some(node)=campaign.project.upgrade(){let mut projects=shared.canonical_projects.lock().unwrap_or_else(|p|p.into_inner());
            if projects.get(&node.alias).is_some_and(|original|Arc::ptr_eq(original,campaign)){projects.remove(&node.alias);}}
    }
}

fn record_original_join(worker:&OwnedWorkerJoin,joined:&mut Option<WorkerJoined>,charged:&mut bool,active:&AtomicUsize)->Result<bool>{
    if joined.is_some(){return Ok(true)}worker.start_join_observer()?;let Some(proof)=worker.try_joined()? else{return Ok(false)};
    *joined=Some(proof);if *charged{*charged=false;active.fetch_sub(1,Ordering::AcqRel);}Ok(true)
}

impl ManagedIngressControl {
    /// Reconcile only the SAME retained intent after actual outer/TLS join.
    /// Historical observation cannot run gates, republish or mint a timely ACK.
    pub(crate) fn reconcile_canonical_off_app(&self,key:&str,original:&FinalizationBudget)->Result<()> {
        ensure!(self.app_thread!=thread::current().id(),"canonical observation requires off App");original.remaining()?;
        let job=self.shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).get(key).cloned().context("original canonical task absent")?;
        let campaign=job.carrier.try_lock().map_err(|_|anyhow::anyhow!("original task borrowed"))?.canonical.clone().context("original canonical campaign absent")?;
        let mut held=campaign.state.try_lock().map_err(|_|anyhow::anyhow!("original canonical worker borrowed"))?;
        ensure!(held.joined.is_some(),"original canonical callback/TLS not joined");
        if held.work.state_data()!=CanonicalState::Recovery{return Ok(())}
        match held.work.observe_only(original){Ok(())=>Ok(()),Err(error)=>{held.observation_error=Some(format!("{error:#}"));Err(error)}}
    }
}

#[cfg(test)]
#[path="managed_canonical_worker_tests.rs"]mod tests;

impl Campaign {
    pub(super) fn observe_original_producers(&self)->Result<Option<crate::managed_machine_entry::census::RuntimeProducerCensusData>> {
        use crate::managed_machine_entry::census::{self,RuntimeProducerCensusData,WorkerCensus};
        let (worker,cleanup,settled)={let Some(held)=census::borrow(&self.state)?else{return Ok(None)};
            (held.worker.clone(),held.sdk_cleanup.clone(),held.work.may_release_serial_slot())};
        let mut data=RuntimeProducerCensusData{canonical_campaigns:1,unsettled_canonical_campaigns:usize::from(!settled),
            canonical_workers:WorkerCensus::observe(worker.as_ref())?,..RuntimeProducerCensusData::default()};
        if let Some(cleanup)=cleanup {
            let Some(actual)=cleanup.observe_original_producer_worker()?else{return Ok(None)};data.canonical_sdk_cleanups=actual;
        }
        Ok(Some(data))
    }
}