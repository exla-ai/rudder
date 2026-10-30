use super::*;
use crate::managed_launch::{ManagedRunSource, ManagedLaunchFence};
use rudder_engine::{journal::OwnerFence, protocol::{ObjectKind, ObjectRef}};

fn source(run_id: &str) -> ManagedRunSource {
    ManagedRunSource::Reserved { fence: ManagedLaunchFence { owner: OwnerFence { project_id: "p".into(), owner_id: "owner".into(), epoch: 1 },
        run_id: run_id.into(), run_epoch: 1, attempt_id: uuid::Uuid::new_v4().to_string(), request_id: uuid::Uuid::new_v4().to_string() },
        runnable_root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"retained fixture root") }
}

#[test]
fn managed_local_source_blocks_checkout_resume_merge_diff_and_final_gate() {
    let mut app = App::new();
    fs::create_dir_all(&app.cwd).unwrap();
    let mut run = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({
        "id":"managed","task":"explicit local root","mode":"execute","status":"done",
        "session":{"nativeSessionId":"saved-thread"}, "managedLocal":source("managed") })).unwrap();
    run.status = AgentStatus::Done; run.node_id = Some("n0".into()); run.plan_id = Some(app.plan().id.clone());
    app.agents.push(run); app.selected_agent = 0;
    assert!(!crate::launch::can_resume_agent(&app.agents[0]));
    assert!(!app.reopen_conversation_at(0));
    assert!(!app.spawn_claude_resume_for(0, &MigratedAgent { run_id:"managed".into(), session_id:"saved-thread".into(), workspace_path:app.cwd.clone(), fresh_prompt:None }));
    assert!(!app.restart_agent_at(0, "replacement", None));
    assert!(app.merge_agent_at(0).unwrap_err().to_string().contains("managed source acceptance"));
    app.toggle_diff_panel(); assert!(!app.diff_panel.open);
    app.plan_mut().planned_nodes.clear(); app.plan_mut().awaiting_approval = false;
    app.plan_mut().plan_launched_node_ids.insert("n0".into());
    app.plan_mut().plan_merged_node_ids.insert("n0".into());
    app.maybe_start_final_gate();
    assert_eq!(app.plan().final_gate_status, FinalGateStatus::Idle);
    let _ = fs::remove_dir_all(&app.cwd);
}

#[test]
#[cfg(target_os = "linux")]
fn managed_local_stop_without_owned_control_stays_orphaned_and_never_resumes() {
    let mut app = App::new(); fs::create_dir_all(&app.cwd).unwrap();
    let run = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({
        "id":"managed","task":"explicit local root","mode":"execute","status":"running","managedLocal":source("managed") })).unwrap();
    app.agents.push(run);
    assert!(app.stop_agent_at(0));
    assert_eq!(app.agents[0].status, AgentStatus::Orphaned);
    assert!(app.agents[0].terminal.is_none());
    assert!(!app.restart_agent_at(0, "replacement", None));
    let _ = fs::remove_dir_all(&app.cwd);
}

#[test]
fn managed_local_restored_marker_with_old_jj_id_cannot_publish_a_row_or_plan_stack() {
    let mut app = App::new(); fs::create_dir_all(&app.cwd).unwrap();
    let plan = app.plan().id.clone();
    let mut managed = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({"id":"managed","task":"owned", "mode":"execute",
        "status":"done", "managedLocal":null, "workspace":{"jjChangeId":"old-legacy-change","path":app.cwd}})).unwrap();
    managed.node_id = Some("n1".into()); managed.plan_id = Some(plan.clone());
    assert_eq!(managed.managed_local, Some(ManagedRunSource::Quarantined));
    assert!(crate::publish::publish_subject_for(&managed, "main").is_none());
    let mut sibling = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({"id":"legacy","task":"owned sibling", "mode":"execute", "status":"done",
        "workspace":{"jjChangeId":"valid-legacy-change","path":app.cwd}})).unwrap();
    sibling.node_id = Some("n0".into()); sibling.plan_id = Some(plan);
    app.agents = vec![managed, sibling];
    for index in [0, 1] { app.publish_agent_at(index); assert!(app.notice.as_deref().unwrap().contains("managed source acceptance")); }
    assert!(!crate::publish::publish_acceptance_path(&app.cwd).exists());
    let _ = fs::remove_dir_all(&app.cwd);
}
