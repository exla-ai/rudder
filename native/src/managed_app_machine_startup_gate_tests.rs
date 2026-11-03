// A denial-only gate plus real local Journal/report IO. No Root/kernel/PG
// owner, helper readiness or backend completion is fabricated here.
#[test]
fn closed_startup_gate_keeps_existing_report_and_receipt_polling_live(){
    let mut app=App::new();fs::create_dir_all(&app.cwd).unwrap();let mut journal=Journal::open(app.cwd.join("journal.sqlite")).unwrap();
    let owner=journal.create_project("kernel-caller-report-fixture","owner","base",0).unwrap();let attempt=uuid::Uuid::new_v4().to_string();let epoch=journal.observe_run_attempt(&owner,"run",&attempt,1).unwrap();
    let fence=ManagedLaunchFence{owner:owner.clone(),run_id:"run".into(),run_epoch:epoch,attempt_id:attempt,request_id:uuid::Uuid::new_v4().to_string()};
    let marker=ManagedRunSource::Reserved{fence:fence.clone(),runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot,b"untrusted-report-data-only")};
    let mut run=crate::gitio::agent_from_run_record(&app.cwd,serde_json::json!({"id":"run","task":"existing report receiver","managedLocal":marker})).unwrap();run.managed_local=Some(marker);run.status=AgentStatus::Running;app.agents.push(run);
    let reports=app.cwd.join(".rudder-reports");fs::create_dir_all(&reports).unwrap();let paths=managed_launch::ManagedReportMapping{host_done_file:reports.join("done.json"),host_signal_file:reports.join("signal.json"),guest_done_file:"/home/agent/.rudder-reports/done.json".into(),guest_signal_file:"/home/agent/.rudder-reports/signal.json".into()};
    fs::write(&paths.host_done_file,br#"{"summary":"existing private work report","run_id":"run"}"#).unwrap();
    let mut state=ManagedAppState::default();assert!(state.queue.send.send(WorkerResult{fence,result:Ok(WorkerStep::Reports {reports:read_reports(&paths),renewal_stop:false})}).is_ok());
    let gate=crate::managed_machine_entry::ManagedMachineStartupGate::closed_for_tests();assert!(gate.check_on_app().is_err());
    state.poll_gated(&mut app,&mut journal,&owner,Some(&gate)).unwrap();
    assert_eq!(app.agents[0].done_summary.as_deref(),Some("existing private work report"));assert_eq!(app.agents[0].status,AgentStatus::Running);assert!(app.agents[0].completed_at.is_none());assert!(state.live.is_empty());
    let _=fs::remove_dir_all(&app.cwd);
}
