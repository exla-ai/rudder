// Fixed connected off-App caller. Every mutable acquisition/source/factory
// carrier is registered in its node/job BEFORE the effect/catch boundary.
fn run_command(shared:&Arc<Shared>,job:&Arc<Job>)->Result<()> {
    let (original,cancelled)={let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());let original=held.original.clone();let cancelled=held.cancelled.clone();
        time(&original,&cancelled)?;let mut store=shared.receipts.lock().unwrap_or_else(|p|p.into_inner());let store=store.as_mut().unwrap();
        store.start_retaining(held.accepted.as_mut().context("original accepted owner absent")?)?;
        store.claim_dispatch(held.accepted.as_mut().unwrap())?;(original,cancelled)};
    // Metadata-only actual selected profile inspection before creating any
    // project owner/task row/seed use. It never reads inherited credentials or
    // substitutes another profile, and its real child is reaped before return.
    let result=if let Some(rejected)=inspect_profile(shared,job,&original,&cancelled)? {rejected}
    else {match &job.command.operation {
        HostOperation::OpenPreparedProjectV1 {..}=>open_project(shared,job,&original,&cancelled)?,
        HostOperation::StartTaskV1 {..}=>start_task(shared,job,&original,&cancelled)?,
        _=>anyhow::bail!("query cannot be an accepted effect"),
    }};
    let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());held.result=Some(result.clone());
    shared.receipts.lock().unwrap_or_else(|p|p.into_inner()).as_mut().unwrap().finish_retaining(held.accepted.as_mut().unwrap(),&result)?;
    held.settled=true;Ok(())
}
fn inspect_profile(shared:&Shared,job:&Job,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<Option<serde_json::Value>> {
    let (alias,backend,_,profile)=operation_selection(&job.command.operation)?;shared.config.assert_current()?;time(original,cancelled)?;
    let runtime={let node=shared.projects.lock().unwrap_or_else(|p|p.into_inner()).get(alias).cloned();
        node.and_then(|node|{let runtime=node.work.lock().unwrap_or_else(|p|p.into_inner()).runtime.clone();runtime})};
    let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
    let loader=if let Some(runtime)=&runtime {runtime.assert_current(original,cancelled)?;runtime.profile_loader()} else {
        ManagedProfileInspection::capture_retaining(&mut held.profile_inspection,shared.config.clone(),alias,original,cancelled)?;
        held.profile_inspection.as_ref().unwrap().loader()
    };
    let metadata=inspect_selected_managed_task_profile(&loader,profile,native_backend(backend),cancelled,original);
    if let Some(runtime)=&runtime {runtime.assert_current(original,cancelled)?;}else{held.profile_inspection.as_ref().unwrap().current(original,cancelled)?;}
    shared.config.assert_current()?;time(original,cancelled)?;
    Ok(match metadata {Ok(meta) if meta.kind==ManagedProfileCredentialKind::ApiKeyV1=>None,
        Ok(meta)=>Some(serde_json::json!({"phase":"rejected","reason":if meta.kind==ManagedProfileCredentialKind::OAuthV1 {"unsupported_oauth"}else{"unsupported_token"}})),
        Err(_)=>Some(serde_json::json!({"phase":"rejected","reason":"explicit_api_profile_unavailable"}))})
}

fn open_project(shared:&Arc<Shared>,job:&Arc<Job>,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<serde_json::Value> {
    shared.control.kernel_startup_off_app(original,cancelled)?;
    let (alias,backend,model,profile)=operation_selection(&job.command.operation)?;
    let explicit_selection=selected(&job.command.operation)?;
    let existing={shared.projects.lock().unwrap_or_else(|p|p.into_inner()).get(alias).cloned()};
    if let Some(node)=existing{if node.selection!=(backend,model.to_owned(),profile.to_owned()){return Ok(serde_json::json!({"phase":"rejected","reason":"project_selection_conflict"}));}
        ensure!(node.phase.load(Ordering::Acquire)==READY,"original project open pending/recovery; no acquire replay");let ticket=node.work.lock().unwrap_or_else(|p|p.into_inner()).ticket.as_ref().context("actual project ticket absent")?.clone();
        shared.control.ingress_task_services(&ticket,original)?;time(original,cancelled)?;return Ok(serde_json::json!({"phase":"project_ready","project_alias":alias}));}
    let fresh=if crate::managed_fresh_machine::fresh_config(&shared.config){
        let machine=shared.fresh.lock().unwrap_or_else(|p|p.into_inner()).clone();
        let Some(machine)=machine else{return Ok(serde_json::json!({"phase":"rejected","reason":"fresh_source_bootstrap_unavailable"}));};
        let Some(assigned)=machine.assigned_off_app(alias,original)? else{return Ok(serde_json::json!({"phase":"rejected","reason":"fresh_project_pending"}));};
        {let actual=match assigned.try_lock(){Ok(a)=>a,Err(std::sync::TryLockError::WouldBlock)=>return Ok(serde_json::json!({"phase":"rejected","reason":"fresh_project_busy"})),Err(_)=>anyhow::bail!("original fresh assigned capsule poisoned")};
            actual.assert_current_bounded(original,cancelled)?;time(original,cancelled)?;}Some(assigned)
    }else{None};
    let node={let mut nodes=shared.projects.lock().unwrap_or_else(|p|p.into_inner());
        if let Some(node)=nodes.get(alias) {if node.selection!=(backend,model.to_owned(),profile.to_owned()){return Ok(serde_json::json!({"phase":"rejected","reason":"project_selection_conflict"}));}
            ensure!(node.phase.load(Ordering::Acquire)==READY,"original project open pending/recovery; no acquire replay");return Ok(serde_json::json!({"phase":"project_ready","project_alias":alias}));}
        if nodes.len()>=PROJECT_LIMIT{return Ok(serde_json::json!({"phase":"rejected","reason":"project_capacity"}));}
        let node=Arc::new(ProjectNode {alias:alias.to_owned(),selection:(backend,model.to_owned(),profile.to_owned()),phase:AtomicU8::new(OPENING),controller:Arc::new(ManagedTaskController::new()),work:Mutex::new(ProjectWork {
            fresh:fresh.clone(),fresh_engine:None,acquisition:None,runtime_capture:None,runtime:None,source:None,ticket:None,seed:None,cache:None,cache_handle:None,cache_outcome:None,supervisor:None,project:None})});
        nodes.insert(alias.to_owned(),node.clone());node};
    job.carrier.lock().unwrap_or_else(|p|p.into_inner()).node=Some(node.clone());
    let result=(||->Result<serde_json::Value> {
        let mut held=node.work.lock().unwrap_or_else(|p|p.into_inner());
        if let Some(actual)=held.fresh.clone(){
            let source=actual.lock().unwrap_or_else(|p|p.into_inner()).source().clone();
            ManagedHostRuntime::capture_from_fresh_retaining(&mut held.runtime_capture,source.environment()?,alias,original,cancelled)?;
        }else{ManagedHostRuntime::capture_retaining(&mut held.runtime_capture,shared.config.clone(),alias,original,cancelled)?;}
        held.runtime=Some(Arc::new(held.runtime_capture.take().unwrap()));let runtime=held.runtime.as_ref().unwrap().clone();runtime.programs(backend)?;
        if let Some(actual)=held.fresh.clone(){
            actual.lock().unwrap_or_else(|p|p.into_inner()).take_engine_retaining(&mut held.fresh_engine,original,cancelled)?;
        }else{
            RetainedManagedAcquisition::capture_retaining(&mut held.acquisition,shared.config.clone(),alias,original,cancelled.clone())?;
            held.acquisition.as_mut().unwrap().acquire_prepared_retaining()?;
            held.fresh_engine=Some(held.acquisition.as_mut().unwrap().take_acknowledged()?);
        }
        // All fallible selection checks precede the pure actual-owner move.
        let display=ManagedProjectDisplay {backend:native_backend(backend),model:model.to_owned(),effort:None,session_started_iso:crate::now_stamp()};
        let ProjectWork{ticket,fresh_engine,..}=&mut *held;ProjectTicket::capture_retaining(ticket,fresh_engine,explicit_selection,display,original.clone(),cancelled.clone())?;
        // SAME ticket exists in this node before queue/refusal; returned owner
        // is retained even if the machine also registered it before queue Err.
        let ticket=held.ticket.as_ref().unwrap().clone();if let Err((actual,error))=shared.control.submit_project(ticket) {held.ticket=Some(actual);return Err(error);}
        let ticket=held.ticket.as_ref().unwrap().clone();wait_ready(original,cancelled,||Ok((ticket.ingress_ready(),ticket.ingress_recovery())))?;
        let (project,supervisor)=shared.control.ingress_task_services(&ticket,original)?;held.project=Some(project);held.supervisor=Some(supervisor);
        runtime.assert_current(original,cancelled)?;
        shared.control.capture_ingress_initial_source_retaining(&mut held.source,&ticket,runtime.store().clone(),original)?;
        if let Some(actual)=held.fresh.clone(){let source=actual.lock().unwrap_or_else(|p|p.into_inner()).source().clone();held.source.as_mut().unwrap().adopt_fresh_prepared_retaining(source,original,cancelled)?;}
        else{match shared.config.data().seed_profile.unwrap_or_default(){
            rudder_vcs_bridge::seed::SeedProfile::SmallV1=>held.source.as_mut().unwrap().prepare(runtime.environment()?,&runtime.settings()?,original,cancelled)?,
            rudder_vcs_bridge::seed::SeedProfile::PagedCowV2=>held.source.as_mut().unwrap().prepare_paged_retaining(runtime.environment()?,&runtime.settings()?,original,cancelled.clone())?,
        }}
        held.source.as_ref().unwrap().prepared_current(original,cancelled)?;
        held.seed=Some(SeedTicket::capture(held.source.take().unwrap(),runtime.store().clone(),original.clone(),cancelled.clone()));
        let seed=held.seed.as_ref().unwrap().clone();if let Err((actual,error))=shared.control.submit_seed(seed) {held.seed=Some(actual);return Err(error);}
        let seed=held.seed.as_ref().unwrap().clone();wait_ready(original,cancelled,||Ok((seed.ingress_ready(),seed.ingress_recovery())))?;
        // Optional reuse is an actual registered original cache factory, not a
        // cached-hit bool. The live owner stays independent from prep permits.
        shared.control.admit_cache_retaining(&mut held.cache,&seed,original,cancelled.clone())?;
        let cache=held.cache.as_ref().unwrap().clone();let factory=cache.clone();let control=shared.control.clone();let actual_project=ticket.clone();
        held.cache_outcome=Some(Arc::new(Mutex::new(CacheOutcome {result:None,panic:None})));let outcome=held.cache_outcome.as_ref().unwrap().clone();
        held.cache_handle=Some(thread::Builder::new().name("rudder-ingress-cache".into()).spawn(move||{
            let mut held=outcome.lock().unwrap_or_else(|p|p.into_inner());
            match catch_unwind(AssertUnwindSafe(||->Result<()> {factory.wait_original_factory_registration()?;control.prepare_cache_on_original_factory(&factory,&actual_project)})) {
                Ok(result)=>held.result=Some(result),Err(panic)=>held.panic=Some(panic),
            }
        })?);
        cache.register_factory_retaining(&mut held.cache_handle)?;join_cache_factory(&cache,original,cancelled)?;
        {let outcome=held.cache_outcome.as_ref().unwrap().lock().unwrap_or_else(|p|p.into_inner());
            ensure!(outcome.panic.is_none()&&matches!(outcome.result.as_ref(),Some(Ok(()))),"actual cache constructor failed/unknown; retained original result");}
        cache.ingress_original_ready()?;
        shared.control.register_initial_current_source(&ticket,&seed,&cache,original)?;
        runtime.assert_current(original,cancelled)?;node.phase.store(READY,Ordering::Release);
        Ok(serde_json::json!({"phase":"project_ready","project_alias":alias}))
    })();if result.is_err(){node.phase.store(RECOVERY,Ordering::Release);}result
}
fn wait_ready(original:&FinalizationBudget,cancelled:&AtomicBool,mut status:impl FnMut()->Result<(bool,bool)>)->Result<()> {
    loop {time(original,cancelled)?;let (ready,recovery)=status()?;ensure!(!recovery,"actual owner requires recovery");if ready{return Ok(());}thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
}
fn start_task(shared:&Arc<Shared>,job:&Arc<Job>,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<serde_json::Value> {
    shared.control.kernel_startup_off_app(original,cancelled)?;
    let (alias,backend,_,_)=operation_selection(&job.command.operation)?;
    let node=shared.projects.lock().unwrap_or_else(|p|p.into_inner()).get(alias).cloned();
    let Some(node)=node else {return Ok(serde_json::json!({"phase":"rejected","reason":"project_not_open"}));};
    if node.phase.load(Ordering::Acquire)!=READY {return Ok(serde_json::json!({"phase":"rejected","reason":"project_not_ready"}));}
    job.carrier.lock().unwrap_or_else(|p|p.into_inner()).node=Some(node.clone());
    let (runtime,project,supervisor,ticket)={let held=node.work.lock().unwrap_or_else(|p|p.into_inner());
        (held.runtime.as_ref().unwrap().clone(),held.project.as_ref().unwrap().clone(),held.supervisor.as_ref().unwrap().clone(),held.ticket.as_ref().unwrap().clone())};
    let (seed,cache)=match shared.control.current_source_for_task(&ticket,original)?{
        crate::managed_machine_entry::CurrentSourceAdmission::Ready{seed,cache}=>(seed,cache),
        crate::managed_machine_entry::CurrentSourceAdmission::Pending=>return Ok(serde_json::json!({"phase":"rejected","reason":"accepted_source_generation_pending"})),
        crate::managed_machine_entry::CurrentSourceAdmission::Fenced=>return Ok(serde_json::json!({"phase":"rejected","reason":"project_shared_admission_fenced"})),
    };
    {let mut held=node.work.lock().unwrap_or_else(|p|p.into_inner());held.seed=Some(seed.clone());held.cache=Some(cache.clone());}
    runtime.assert_current(original,cancelled)?;
    job.step.store(STEP_ADMIT,Ordering::Release);
    if shared.send.try_send(AppEvent::Work(job.clone())).is_err(){job.step.store(STEP_REFUSED,Ordering::Release);return Ok(serde_json::json!({"phase":"rejected","reason":"actual_app_admission_queue_busy"}));}
    if let Err(error)=wait_step(job,STEP_ADMITTED,original,cancelled) {
        let held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
        if let Some(reason)=held.task.refused_before_factory {
            // Actual App result, before any factory/source scope dispatch. The
            // original admission stays charged until this job's real TLS join.
            return Ok(serde_json::json!({"phase":"rejected","reason":reason}));
        }
        return Err(error);
    }
    {
        let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
        let admission=held.task.admission.as_ref().context("actual App admission absent")?;let retention=admission.retain_factory_until_join();
        // Capability is consumed BEFORE any task/fork registration effect.
        held.task.factory_permit.take().context("original factory permit absent")?;
        held.task.factory=Some(Arc::new(Mutex::new(FactoryCarrier {admission:held.task.admission.take(),retention:Some(retention),prepared:None,outcome:None,error:None,panic:None,recorded:false,ready:false})));
        shared.control.admit_task_retaining(&mut held.task.scope,&seed,&ticket,original,cancelled.clone())?;
        let scope=held.task.scope.as_ref().unwrap().clone();let actual_scope=scope.clone();let factory=held.task.factory.as_ref().unwrap().clone();let control=shared.control.clone();let owner=project.clone();let image=runtime.clone();
        held.task.factory_handle=Some(thread::Builder::new().name("rudder-ingress-task-factory".into()).spawn(move||{
            // Actual carrier is OUTSIDE catch. It preserves every error/panic,
            // input/secret/charge even if result delivery or binding fails.
            let mut held=factory.lock().unwrap_or_else(|p|p.into_inner());
            let result=catch_unwind(AssertUnwindSafe(||->Result<()> {
                actual_scope.wait_original_factory_registration()?;
                let (original,cancelled)=held.admission.as_ref().context("original admission absent")?.source_lifetime();image.assert_current(&original,&cancelled)?;
                let admission=held.admission.take().context("original admitted call absent")?;
                let prepared=admission.prepare(&owner,|selected,cancel,budget|Ok(load_selected_managed_task_profile(&image.profile_loader(),selected,cancel,budget)?),
                    |_,cancel,budget|{image.assert_current(budget,cancel)?;let source=control.source_for_factory(&actual_scope,Some(&cache))?;image.resources(source,backend)});
                match prepared {Ok(value)=>{
                    held.prepared=Some(value);actual_scope.bind_prepared_retaining(held.prepared.as_mut().unwrap())?;
                    held.outcome=Some(Ok(held.prepared.take().unwrap()));held.ready=true;
                },Err(error)=>held.outcome=Some(Err(error))}
                actual_scope.retain_original_factory_outcome(&mut held.outcome)?;held.recorded=true;Ok(())
            }));match result {Ok(Ok(()))=>{},Ok(Err(error))=>{
                // Pure late original-thread ownership capture is allowed after
                // timeout, but cannot renew/read/adopt a source or task.
                if held.prepared.is_none()&&!held.recorded {
                    if held.outcome.is_none(){held.outcome=Some(Err(error));}
                    match actual_scope.retain_original_factory_outcome(&mut held.outcome) {Ok(())=>held.recorded=true,Err(capture)=>held.error=Some(capture)}
                }else{held.error=Some(error);}
            },Err(panic)=>held.panic=Some(panic)}
        })?);
        scope.register_factory_retaining(&mut held.task.factory_handle)?;
    }
    let scope=job.carrier.lock().unwrap_or_else(|p|p.into_inner()).task.scope.as_ref().unwrap().clone();
    join_task_factory(&scope,original,cancelled)?;
    let ready={let held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());let factory=held.task.factory.as_ref().unwrap().lock().unwrap_or_else(|p|p.into_inner());
        ensure!(factory.panic.is_none()&&factory.error.is_none()&&factory.recorded,"original factory result/panic requires recovery");factory.ready};
    if !ready||time(original,cancelled).is_err() {
        settle_unused_factory(shared,job,&seed,&scope,&supervisor)?;
        return Ok(serde_json::json!({"phase":"rejected","reason":"original_preparation_refused_or_expired"}));
    }
    job.step.store(STEP_LAUNCH,Ordering::Release);
    if shared.send.try_send(AppEvent::Work(job.clone())).is_err(){
        job.step.store(STEP_REFUSED,Ordering::Release);settle_unused_factory(shared,job,&seed,&scope,&supervisor)?;
        return Ok(serde_json::json!({"phase":"rejected","reason":"actual_app_launch_queue_busy"}));
    }
    wait_step(job,STEP_LAUNCHED,original,cancelled)?;
    let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
    if held.task.started.is_none() {
        scope.retain_undispatched_refusal(&mut held.task.refusal)?;drop(held);
        settle_unused_factory(shared,job,&seed,&scope,&supervisor)?;
        return Ok(serde_json::json!({"phase":"rejected","reason":"actual_app_adoption_refused"}));
    }
    let fence=held.task.started.as_ref().unwrap().clone();held.task.factory.as_ref().unwrap().lock().unwrap_or_else(|p|p.into_inner()).retention=None;
    Ok(serde_json::json!({"phase":"task_queued","project_alias":alias,"run_id":fence.run_id,"run_epoch":fence.run_epoch.to_string(),"attempt_id":fence.attempt_id}))
}
// Existing join APIs use try_lock so the App never waits. This OFF-App caller
// observes the SAME registered original handle under the SAME deadline until
// its real join/TLS proof arrives. No operation or uncertain IO is replayed.
fn join_task_factory(scope:&SeedTaskTicket,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    loop {time(original,cancelled)?;match scope.join_factory_off_app(){Ok(())=>return Ok(()),Err(error)=>{
        if scope.poll_factory_joined_off_app()? {return Ok(());}if time(original,cancelled).is_err(){return Err(error);}
        thread::sleep(original.phase_limit(Duration::from_millis(2))?);
    }}}
}
fn join_cache_factory(cache:&SeedCacheTicket,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    loop {time(original,cancelled)?;match cache.join_factory_off_app(){Ok(())=>return Ok(()),Err(error)=>{
        if cache.ingress_poll_factory_joined()? {return Ok(());}if time(original,cancelled).is_err(){return Err(error);}
        thread::sleep(original.phase_limit(Duration::from_millis(2))?);
    }}}
}
fn settle_unused_factory(shared:&Shared,job:&Job,seed:&SeedTicket,scope:&SeedTaskTicket,supervisor:&Arc<ManagedTaskLifetimeSupervisor>)->Result<()> {
    let (original,cancelled)={let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
        if held.task.cleanup.is_none(){held.task.cleanup=Some((FinalizationBudget::begin(Duration::from_secs(30))?,Arc::new(AtomicBool::new(false))));}
        held.task.cleanup.as_ref().unwrap().clone()};
    ensure!(shared.control.settle_never_started_task_off_app(seed,scope,Some(supervisor),&original,cancelled,false)?==NeverStartedProgress::Joined,"actual unused settlement pending; original owners retained");
    let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
    let factory=held.task.factory.as_ref().unwrap().clone();let mut factory=factory.lock().unwrap_or_else(|p|p.into_inner());
    drop(factory.admission.take());factory.retention=None;Ok(())
}
fn wait_step(job:&Job,wanted:u8,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    loop {time(original,cancelled)?;let state=job.step.load(Ordering::Acquire);ensure!(state!=STEP_REFUSED,"actual App refused; same owners retained");if state==wanted{return Ok(());}thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
}
fn retain_joined_completions(shared:&Arc<Shared>) {
    let jobs:Vec<_>=shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).values().cloned().collect();
    for job in jobs {let mut held=match job.carrier.try_lock(){Ok(h)=>h,Err(_)=>continue};
        if held.joined.is_some(){if let Ok(read)=FinalizationBudget::begin(Duration::from_millis(100)){let TaskWork{scope,enrollment,..}=&mut held.task;let _=shared.control.release_retired_ingress_aliases(scope,enrollment,&read);}}
        if held.task.joined.is_some(){continue;}
        let Some(fence)=held.task.started.as_ref().cloned() else {continue;};let Some(node)=held.node.as_ref().cloned() else {continue;};
        let Ok(project)=node.work.try_lock() else {continue;};let Some(supervisor)=project.supervisor.as_ref() else {continue;};
        if let Ok(Some(joined))=supervisor.take_joined_off_app(&fence){held.task.joined=Some(joined);}
        // Whole original capsule is held for explicit verification/publication
        // or discard. Never automatically accept history or reconstruct ACKs.
    }
}
