// App-only fixed typed messages. No transport/SQLite/profile/Git/model call.
// Existing start_managed_task retains its inherited local Journal/save IO.
fn poll_app_job(job:&Arc<Job>,runtime:&mut ManagedMachineRuntime)->Result<bool> {
    let step=job.step.load(Ordering::Acquire);if !matches!(step,STEP_ADMIT|STEP_LAUNCH){return Ok(true);}
    let mut held=match job.carrier.try_lock(){Ok(h)=>h,Err(_)=>return Ok(false)};
    if time(&held.original,&held.cancelled).is_err(){job.step.store(STEP_REFUSED,Ordering::Release);return Ok(true);}
    let node=held.node.as_ref().context("actual task project absent")?.clone();
    let project=match node.work.try_lock(){Ok(p)=>p,Err(_)=>return Ok(false)};let ticket=project.ticket.as_ref().context("actual project ticket absent")?.clone();drop(project);
    let Some(mut adapter)=runtime.project_adapter(&ticket)? else {return Ok(false);};
    let startup=adapter.startup_gate_on_app();
    let (app,_,_,_)=adapter.task_parts();
    if step==STEP_ADMIT {
        if let Err(error)=startup.check_on_app(){held.error=Some(error);held.task.refused_before_factory=Some("kernel_startup_pending_or_closed");job.step.store(STEP_REFUSED,Ordering::Release);return Ok(true);}
        let HostOperation::StartTaskV1 {backend,model,text,..}=&job.command.operation else {job.step.store(STEP_REFUSED,Ordering::Release);return Ok(true);};
        let run_id=format!("task-{}",held.accepted.as_ref().context("original receipt owner absent")?.receipt.command_id.replace('-',""));
        ensure!(!app.agents.iter().any(|run|run.id==run_id),"original managed row already exists; no replay");
        let mut row=crate::gitio::agent_from_run_record(&app.cwd,serde_json::json!({"id":run_id,"task":text,"model":model,"backend":match backend {HostBackend::Codex=>"codex",HostBackend::Claude=>"claude"},
            "mode":"execute","status":"stopped","createdAt":crate::now_stamp()})).context("new explicit managed row could not be built")?;
        row.mode=AgentMode::Execute;row.status=AgentStatus::Stopped;row.backend=native_backend(*backend);row.model=model.clone();
        if let Err(error)=startup.check_on_app(){held.error=Some(error);held.task.refused_before_factory=Some("kernel_startup_closed_before_row_admission");job.step.store(STEP_REFUSED,Ordering::Release);return Ok(true);}
        app.agents.push(row);let index=app.agents.len()-1;
        let admission=node.controller.admit_original(app,index,selected(&job.command.operation)?,&held.original,held.cancelled.clone());
        match admission {Ok(admission)=>{held.task.admission=Some(admission);held.task.factory_permit=Some(FactoryPermit);},Err(error)=>{
            held.error=Some(error);held.task.refused_before_factory=Some("actual_app_admission_refused");job.step.store(STEP_REFUSED,Ordering::Release);return Ok(true);}}
        // The actual admission already belongs to its original bounded carrier
        // BEFORE this inherited local record save can fail.
        if let Err(error)=crate::save_native_run_record(&app.cwd,&app.agents[index]){held.error=Some(error);held.task.refused_before_factory=Some("actual_run_record_write_refused");job.step.store(STEP_REFUSED,Ordering::Release);return Ok(true);}
        job.step.store(STEP_ADMITTED,Ordering::Release);return Ok(true);
    }
    let scope=held.task.scope.as_ref().context("original scoped factory absent")?.clone();
    let run_id=format!("task-{}",held.accepted.as_ref().unwrap().receipt.command_id.replace('-',""));
    let index=app.agents.iter().position(|run|run.id==run_id).context("original stopped task row disappeared")?;
    let prepared=scope.take_original_prepared_on_app()?.context("original successful prepared outcome unavailable")?;
    match adapter.start_scoped_task(&scope,index,prepared) {Ok(fence)=>held.task.started=Some(fence),Err(refusal)=>held.task.refusal=Some(refusal)}
    job.step.store(STEP_LAUNCHED,Ordering::Release);Ok(true)
}
fn poll_app_enrollment(shared:&Shared,job:&Arc<Job>,runtime:&mut ManagedMachineRuntime)->Result<()> {
    let mut held=match job.carrier.try_lock(){Ok(h)=>h,Err(_)=>return Ok(())};
    if held.task.released||held.task.joined.is_some(){return Ok(());}
    let Some(fence)=held.task.started.as_ref().cloned() else {return Ok(());};
    let Some(node)=held.node.as_ref().cloned() else {return Ok(());};let project=match node.work.try_lock(){Ok(p)=>p,Err(_)=>return Ok(())};
    let Some(ticket)=project.ticket.as_ref().cloned() else {return Ok(());};drop(project);
    let Some(mut adapter)=runtime.project_adapter(&ticket)? else {return Ok(());};
    if held.task.enrollment.is_none() {
        let (app,_,_,_)=adapter.task_parts();
        // An Err is pending/stale/uncertain DATA, never evidence of absence or
        // cleanup. Original budget, helper/pane Arc and start barrier stay held.
        if held.task.pending_helper.is_none(){let Ok(pending)=app.managed_task_pending_helper(&fence) else {return Ok(());};held.task.pending_helper=Some(pending);}
        let pending=held.task.pending_helper.take().unwrap();
        match EnrollmentTicket::capture_scoped(held.task.scope.as_ref().context("actual task scope absent")?,pending) {
            Ok(enrollment)=>held.task.enrollment=Some(enrollment),Err((pending,error))=>{
                // Pure capture may retry SAME retained pending ownership within
                // its original budget. It cannot replay an enrollment request.
                held.task.pending_helper=Some(pending);held.error=Some(error);return Ok(());
            }
        }
    }
    if !held.task.enrollment_submitted {
        let ticket=held.task.enrollment.as_ref().unwrap().clone();
        match shared.control.submit_enrollment(ticket) {Ok(actual)=>{held.task.enrollment=Some(actual);held.task.enrollment_submitted=true;},
            Err((actual,error))=>{held.task.enrollment_submitted=!actual.ingress_queue_retryable();held.task.enrollment=Some(actual);held.error=Some(error);return Ok(());}}
    }
    if adapter.release_enrollment_retaining(held.task.enrollment.as_ref().unwrap())? {held.task.released=true;}
    Ok(())
}

// Fresh bounded observation from the actual App loop. No saved receipt, absent
// process, request reply, or status word is promoted into any authority.
fn poll_app_observation(request:&Arc<ObservationRequest>,runtime:&mut ManagedMachineRuntime)->Result<bool> {
    if request.original.remaining().is_err(){request.done.store(true,Ordering::Release);return Ok(true);}
    let held=match request.job.carrier.try_lock(){Ok(held)=>held,Err(_)=>return Ok(false)};
    let Some(node)=held.node.as_ref().cloned() else {request.done.store(true,Ordering::Release);return Ok(true);};
    let Some(fence)=held.task.started.as_ref().cloned() else {request.done.store(true,Ordering::Release);return Ok(true);};
    let project=match node.work.try_lock(){Ok(project)=>project,Err(_)=>return Ok(false)};
    let Some(ticket)=project.ticket.as_ref().cloned() else {request.done.store(true,Ordering::Release);return Ok(true);};drop(project);
    let Some(mut adapter)=runtime.project_adapter(&ticket)? else {return Ok(false);};
    let (app,_,_,_)=adapter.task_parts();let observation=app.managed_task_ingress_observation(&fence);
    let value=observation.map(|mut value|{value["command_id"]=serde_json::Value::String(held.accepted.as_ref().unwrap().receipt.command_id.clone());value});
    if request.original.remaining().is_ok(){match request.result.try_lock(){Ok(mut result)=>*result=value,Err(_)=>return Ok(false)}}
    request.done.store(true,Ordering::Release);Ok(true)
}
