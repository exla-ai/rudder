#[test]
fn an_absent_process_or_done_display_row_cannot_create_managed_observation_custody() {
    let mut app=App::new_managed(crate::ManagedAppInitialState{repository:PathBuf::from("/unused-ingress-observation-fixture"),selection:crate::InitialSelection{backend:Backend::Codex,model:"selected".into(),effort:None},branch:"known-head-DATA".into(),session_started_iso:"2026-11-01T00:00:00Z".into()});
    let mut row=crate::gitio::agent_from_run_record(&app.cwd,serde_json::json!({"id":"original-task","task":"fixture","model":"selected","backend":"codex","mode":"execute","status":"done"})).unwrap();row.status=AgentStatus::Done;row.terminal=None;app.agents.push(row);
    let fence=ManagedLaunchFence{owner:OwnerFence{project_id:"fixture-data-only".into(),owner_id:"not-an-owner".into(),epoch:1},run_id:"original-task".into(),run_epoch:u64::MAX,attempt_id:uuid::Uuid::new_v4().to_string(),request_id:uuid::Uuid::new_v4().to_string()};
    assert!(app.managed_task_ingress_observation(&fence).is_none(),"saved identity/status or absent child cannot mint an original managed-task observation");
    assert!(app.request_managed_task_ingress_stop(&fence,&fence.owner).is_err(),"an absent actual task control cannot be reconstructed from a saved row");
    assert_eq!(app.agents[0].status,AgentStatus::Done);
}
#[test]
fn exact_managed_row_without_original_live_control_cannot_confirm_stop() {
    let mut app=App::new_managed(crate::ManagedAppInitialState{repository:PathBuf::from("/unused-ingress-stop-fixture"),selection:crate::InitialSelection{backend:Backend::Codex,model:"selected".into(),effort:None},branch:"head-DATA".into(),session_started_iso:"2026-11-01T00:00:00Z".into()});
    let fence=ManagedLaunchFence{owner:OwnerFence{project_id:"data-only".into(),owner_id:"not-an-owner".into(),epoch:1},run_id:"original-task".into(),run_epoch:7,attempt_id:uuid::Uuid::new_v4().to_string(),request_id:uuid::Uuid::new_v4().to_string()};
    let marker=ManagedRunSource::Reserved{fence:fence.clone(),runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot,b"untrusted-row-DATA")};
    let mut row=crate::gitio::agent_from_run_record(&app.cwd,serde_json::json!({"id":"original-task","task":"fixture","managedLocal":marker})).unwrap();
    row.managed_local=Some(marker);row.status=AgentStatus::Running;app.agents.push(row);
    let mut foreign=fence.owner.clone();foreign.epoch+=1;
    assert!(app.request_managed_task_ingress_stop(&fence,&foreign).is_err());
    assert!(app.request_managed_task_ingress_stop(&fence,&fence.owner).is_err());
    assert_eq!(app.agents[0].status,AgentStatus::Running);assert!(app.managed_local.live.is_empty());assert!(app.managed_local.pending.is_empty());
    assert_eq!(app.managed_task_ingress_observation(&fence).unwrap()["stop_requested_observed"],serde_json::Value::Null);
}
