#[test]
fn an_absent_process_or_done_display_row_cannot_create_managed_observation_custody() {
    let mut app=App::new_managed(crate::ManagedAppInitialState{repository:PathBuf::from("/unused-ingress-observation-fixture"),selection:crate::InitialSelection{backend:Backend::Codex,model:"selected".into(),effort:None},branch:"known-head-DATA".into(),session_started_iso:"2026-11-01T00:00:00Z".into()});
    let mut row=crate::gitio::agent_from_run_record(&app.cwd,serde_json::json!({"id":"original-task","task":"fixture","model":"selected","backend":"codex","mode":"execute","status":"done"})).unwrap();row.status=AgentStatus::Done;row.terminal=None;app.agents.push(row);
    let fence=ManagedLaunchFence{owner:OwnerFence{project_id:"fixture-data-only".into(),owner_id:"not-an-owner".into(),epoch:1},run_id:"original-task".into(),run_epoch:u64::MAX,attempt_id:uuid::Uuid::new_v4().to_string(),request_id:uuid::Uuid::new_v4().to_string()};
    assert!(app.managed_task_ingress_observation(&fence).is_none(),"saved identity/status or absent child cannot mint an original managed-task observation");
}
