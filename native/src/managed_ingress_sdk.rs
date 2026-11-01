//! Authenticated ordinary SDK command on the EXISTING host worker pool.
//! No scheduler, model selection, fake task completion or helper entrypoint.
use super::*;
use crate::managed_machine_entry::{CurrentSourceSdkCampaign,CurrentSourceSdkCleanup};
use rudder_native::host_ingress::SdkBuildCase;
const HISTORY_PER_PROJECT:usize=8;
pub(super) struct Slot {job:std::sync::Weak<Job>,state:Mutex<State>,cleanup_owned:AtomicBool}
struct State {campaign:Option<Arc<Mutex<CurrentSourceSdkCampaign>>>,build_joined:Option<WorkerJoined>,cleanup:Option<Arc<Mutex<CurrentSourceSdkCleanup>>>,
    cleanup_budget:Option<FinalizationBudget>,cleanup_cancel:Arc<AtomicBool>,cleanup_worker:Option<OwnedWorkerJoin>,cleanup_joined:Option<WorkerJoined>,
    charged:bool,attempted:bool,settled:bool,join_error:Option<anyhow::Error>,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
pub(super) fn is_sdk(op:&HostOperation)->bool{matches!(op,HostOperation::BuildCurrentSourceSdkV1{..})}
pub(super) fn alias(op:&HostOperation)->Result<&str>{match op{HostOperation::BuildCurrentSourceSdkV1{project_alias,..}=>Ok(project_alias),_=>anyhow::bail!("not SDK command")}}
pub(super) fn available(shared:&Shared,op:&HostOperation)->bool{let Ok(alias)=alias(op)else{return true};
    if shared.sdk_projects.lock().unwrap_or_else(|p|p.into_inner()).contains_key(alias){return false;}
    shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).values().filter(|j|matches!(&j.command.operation,HostOperation::BuildCurrentSourceSdkV1{project_alias,..}if project_alias==alias)).count()<HISTORY_PER_PROJECT
}
pub(super) fn reserve(shared:&Shared,job:&Arc<Job>)->Result<()> {if !is_sdk(&job.command.operation){return Ok(())}let alias=alias(&job.command.operation)?.to_owned();
    let slot=Arc::new(Slot{job:Arc::downgrade(job),cleanup_owned:AtomicBool::new(false),state:Mutex::new(State{campaign:None,build_joined:None,cleanup:None,cleanup_budget:None,cleanup_cancel:Arc::new(AtomicBool::new(false)),cleanup_worker:None,cleanup_joined:None,charged:false,attempted:false,settled:false,join_error:None,error:None,panic:None})});
    let mut projects=shared.sdk_projects.lock().unwrap_or_else(|p|p.into_inner());ensure!(!projects.contains_key(&alias),"SDK project already has original unresolved campaign");projects.insert(alias,slot.clone());job.carrier.lock().unwrap_or_else(|p|p.into_inner()).sdk=Some(slot);Ok(())
}
pub(super) fn run_original(shared:&Arc<Shared>,job:&Arc<Job>,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<serde_json::Value>{
    let HostOperation::BuildCurrentSourceSdkV1{project_alias,case,repeat_identical}=&job.command.operation else{anyhow::bail!("not original SDK operation")};
    shared.config.assert_current()?;ensure!(shared.config.data().compiler_sdk.is_some()&&shared.config.data().runtime_qualification.is_none()&&shared.config.data().compiler_qualification.is_none(),"SDK command needs actual configured ordinary SDK");
    let node=open_source_project(shared,job,project_alias,original,cancelled)?;let ticket=original_lock(&node.work,original)?.ticket.as_ref().context("actual source-only ticket absent")?.clone();
    let slot=job.carrier.lock().unwrap_or_else(|p|p.into_inner()).sdk.as_ref().context("original SDK admission absent")?.clone();
    let campaign={let mut held=original_lock(&slot.state,original)?;ensure!(held.campaign.is_none(),"original SDK command cannot replay");let mut out=None;
        shared.control.capture_current_sdk_case_campaign_retaining(&mut out,ticket,*case,original.clone(),cancelled.clone())?;let actual=Arc::new(Mutex::new(out.unwrap()));held.campaign=Some(actual.clone());actual};
    let mut actual=original_lock(&campaign,original)?;let result=actual.run_off_app_retaining(*repeat_identical);
    match result {Ok(())=>Ok(serde_json::json!({"phase":"sdk_artifacts_published","artifacts":actual.artifact_roots_data()?,"case":case,"cache_reuse":false,"source_accepted":false,"job_disposal":"pending"})),
        Err(error)=>{let mut failed=false;let mut unknown=false;let mut artifacts=Vec::new();
            for build in actual.retained_builds(){if let Ok(outcome)=build.outcome(){artifacts.push(outcome.artifacts()?);}else if build.terminal_failure().is_ok(){failed=true;}else{unknown=true;}}
            if failed&&!unknown{Ok(serde_json::json!({"phase":"sdk_helper_failed","case":case,"cache_reuse":false,"artifacts":artifacts,"source_accepted":false,"job_disposal":"pending"}))}else{Err(error)}}}
}
fn open_source_project(shared:&Arc<Shared>,job:&Arc<Job>,alias:&str,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<Arc<ProjectNode>> {
    shared.control.kernel_startup_off_app(original,cancelled)?;
    if let Some(node)=shared.projects.lock().unwrap_or_else(|p|p.into_inner()).get(alias).cloned(){ensure!(node.selection.is_none()&&node.phase.load(Ordering::Acquire)==READY,"SDK source-only project pending/recovery or model purpose; no takeover");job.carrier.lock().unwrap_or_else(|p|p.into_inner()).node=Some(node.clone());return Ok(node)}
    let machine=shared.fresh.lock().unwrap_or_else(|p|p.into_inner()).clone().context("actual fresh source bootstrap absent")?;
    let assigned=machine.assigned_off_app(alias,original)?.context("actual PG source assignment pending; no local/preparation fallback")?;
    {let held=original_lock(&assigned,original)?;held.assert_current_bounded(original,cancelled)?;}
    let node=Arc::new(ProjectNode{alias:alias.into(),selection:None,phase:AtomicU8::new(OPENING),controller:Arc::new(ManagedTaskController::new()),work:Mutex::new(ProjectWork{fresh:Some(assigned),fresh_engine:None,acquisition:None,runtime_capture:None,runtime:None,source:None,ticket:None,seed:None,cache:None,cache_handle:None,cache_outcome:None,supervisor:None,project:None})});
    {let mut projects=shared.projects.lock().unwrap_or_else(|p|p.into_inner());ensure!(projects.len()<PROJECT_LIMIT&&!projects.contains_key(alias),"project slot occupied; no original replacement");projects.insert(alias.into(),node.clone());}
    job.carrier.lock().unwrap_or_else(|p|p.into_inner()).node=Some(node.clone());
    let result=(||->Result<()>{let mut held=original_lock(&node.work,original)?;let assigned=held.fresh.as_ref().unwrap().clone();let source=original_lock(&assigned,original)?.source().clone();
        ManagedHostRuntime::capture_from_fresh_retaining(&mut held.runtime_capture,source.environment()?,alias,original,cancelled)?;
        held.runtime=Some(Arc::new(held.runtime_capture.take().unwrap()));let runtime=held.runtime.as_ref().unwrap().clone();
        original_lock(&assigned,original)?.take_engine_retaining(&mut held.fresh_engine,original,cancelled)?;
        let ProjectWork{ticket,fresh_engine,..}=&mut *held;ProjectTicket::capture_source_only_retaining(ticket,fresh_engine,original.clone(),cancelled.clone())?;
        let ticket=held.ticket.as_ref().unwrap().clone();if let Err((original_ticket,error))=shared.control.submit_project(ticket){held.ticket=Some(original_ticket);return Err(error)}
        let ticket=held.ticket.as_ref().unwrap().clone();wait_ready(original,cancelled,||Ok((ticket.ingress_ready(),ticket.ingress_recovery())))?;
        shared.control.capture_ingress_initial_source_retaining(&mut held.source,&ticket,runtime.store().clone(),original)?;
        held.source.as_mut().unwrap().adopt_fresh_prepared_retaining(source,original,cancelled)?;held.source.as_ref().unwrap().prepared_current(original,cancelled)?;
        held.seed=Some(SeedTicket::capture(held.source.take().unwrap(),runtime.store().clone(),original.clone(),cancelled.clone()));let seed=held.seed.as_ref().unwrap().clone();
        if let Err((original_seed,error))=shared.control.submit_seed(seed){held.seed=Some(original_seed);return Err(error)}let seed=held.seed.as_ref().unwrap().clone();wait_ready(original,cancelled,||Ok((seed.ingress_ready(),seed.ingress_recovery())))?;
        shared.control.admit_cache_retaining(&mut held.cache,&seed,original,cancelled.clone())?;
        let cache=held.cache.as_ref().unwrap().clone();let factory=cache.clone();let control=shared.control.clone();let actual_project=ticket.clone();held.cache_outcome=Some(Arc::new(Mutex::new(CacheOutcome{result:None,panic:None})));let outcome=held.cache_outcome.as_ref().unwrap().clone();
        held.cache_handle=Some(thread::Builder::new().name("rudder-sdk-source-cache".into()).spawn(move||{let mut out=outcome.lock().unwrap_or_else(|p|p.into_inner());match catch_unwind(AssertUnwindSafe(||{factory.wait_original_factory_registration()?;control.prepare_cache_on_original_factory(&factory,&actual_project)})){Ok(result)=>out.result=Some(result),Err(panic)=>out.panic=Some(panic)}})?);
        cache.register_factory_retaining(&mut held.cache_handle)?;join_cache_factory(&cache,original,cancelled)?;
        {let outcome=held.cache_outcome.as_ref().unwrap().lock().unwrap_or_else(|p|p.into_inner());ensure!(outcome.panic.is_none()&&matches!(outcome.result.as_ref(),Some(Ok(()))),"actual source cache factory failed/unknown");}
        cache.ingress_original_ready()?;shared.control.register_initial_current_source(&ticket,&seed,&cache,original)?;runtime.assert_current(original,cancelled)?;time(original,cancelled)?;Ok(())})();
    if let Err(error)=result{node.phase.store(RECOVERY,Ordering::Release);return Err(error)}node.phase.store(READY,Ordering::Release);Ok(node)
}
pub(super) fn poll(shared:&Arc<Shared>) {
    let slots:Vec<_>=shared.sdk_projects.lock().unwrap_or_else(|p|p.into_inner()).iter().map(|(a,s)|(a.clone(),s.clone())).collect();
    for(alias,slot)in slots {
        let Some(job)=slot.job.upgrade()else{continue};
        let joined={let Ok(held)=job.carrier.try_lock()else{continue};held.joined.clone()};let Some(joined)=joined else{continue};
        let mut state=match slot.state.try_lock(){Ok(s)=>s,Err(_)=>continue};if state.settled{continue;}
        if state.build_joined.is_none(){state.build_joined=Some(joined);}
        // Capture exactly once BEFORE any pool wait/queue, never after admission.
        if state.cleanup_budget.is_none(){match FinalizationBudget::begin(Duration::from_secs(30)){Ok(b)=>state.cleanup_budget=Some(b),Err(e)=>{state.error=Some(e);continue}}}
        if state.cleanup_worker.is_none() {
            if state.error.is_some()||state.panic.is_some()||state.attempted{continue;}
            if state.cleanup.is_none(){let Some(campaign)=state.campaign.clone()else{continue};let mut cleanup=None;
                let result=CurrentSourceSdkCleanup::capture_retaining(&mut cleanup,campaign,state.build_joined.as_ref().unwrap().clone(),state.cleanup_budget.as_ref().unwrap().clone(),state.cleanup_cancel.clone());
                if let Err(e)=result{state.error=Some(e);continue;}state.cleanup=Some(Arc::new(Mutex::new(cleanup.unwrap())));}
            if shared.active.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|n.checked_add(1).filter(|v|*v<=WORKERS)).is_err(){continue;}
            state.charged=true;state.attempted=true;let actual=slot.clone();let cleanup=state.cleanup.as_ref().unwrap().clone();
            let started=thread::Builder::new().name("rudder-sdk-job-cleanup".into()).spawn(move||{
                while !actual.cleanup_owned.load(Ordering::Acquire){thread::yield_now();}
                let result=catch_unwind(AssertUnwindSafe(||cleanup.lock().unwrap_or_else(|p|p.into_inner()).run_off_app_retaining()));
                let mut held=actual.state.lock().unwrap_or_else(|p|p.into_inner());match result{Ok(Ok(()))=>{},Ok(Err(e))=>held.error=Some(e),Err(p)=>held.panic=Some(p)}
            });
            match started{Ok(worker)=>{state.cleanup_worker=Some(OwnedWorkerJoin::new(worker));slot.cleanup_owned.store(true,Ordering::Release);},
                Err(e)=>{state.error=Some(e.into());state.charged=false;shared.active.fetch_sub(1,Ordering::AcqRel);continue;}}
        }
        // Observe the REAL worker even after callback Err/panic. Neither error
        // nor completion frees its host-pool charge before actual join/TLS.
        if state.cleanup_joined.is_none(){let worker=state.cleanup_worker.as_ref().unwrap();
            if let Err(e)=worker.start_join_observer(){state.join_error=Some(e);continue;}
            match worker.try_joined(){Ok(Some(joined))=>{state.cleanup_joined=Some(joined);if state.charged{state.charged=false;shared.active.fetch_sub(1,Ordering::AcqRel);}},
                Ok(None)=>continue,Err(e)=>{state.join_error=Some(e);continue;}}
        }
        if state.error.is_some()||state.panic.is_some(){continue;}
        let result=state.cleanup.as_ref().unwrap().lock().unwrap_or_else(|p|p.into_inner()).settle_after_cleanup_join_retaining(state.cleanup_joined.as_ref().unwrap().clone());
        if let Err(e)=result{state.error=Some(e);continue;}state.settled=true;drop(state);
        let mut projects=shared.sdk_projects.lock().unwrap_or_else(|p|p.into_inner());if projects.get(&alias).is_some_and(|original|Arc::ptr_eq(original,&slot)){projects.remove(&alias);}
    }
}

/// Pure retained-object observation DATA. Never reconstruct an owner/idle fact
/// from a database row, missing map entry, callback or transport disconnect.
pub(super) fn observation(job:&Arc<Job>)->Option<serde_json::Value>{
    let held=job.carrier.try_lock().ok()?;let accepted=held.accepted.as_ref()?;let slot=held.sdk.as_ref()?;let state=slot.state.try_lock().ok()?;
    Some(serde_json::json!({"command_key":job.command.command_key,"command_id":accepted.receipt.command_id,"project_alias":alias(&job.command.operation).ok()?,
        "build_worker_join_observed":state.build_joined.is_some(),"cleanup_worker_join_observed":state.cleanup_joined.is_some(),
        "job_disposal":if state.settled{"settled"}else if state.error.is_some()||state.panic.is_some(){"retained"}else{"pending"},
        "source_authority":false,"shared_floor_retirement":false}))
}
