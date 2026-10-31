//! Explicit physical fixture for App-owned source helpers. Never default routing.
use super::*;
use crate::{managed_launch::ManagedRunSource, managed_source_app::{self,
    SourceOwnerLifetimeSupervisor}, App, AgentStatus};

fn drive(app: &mut App, owner: &rudder_engine::journal::OwnerFence,
    deadline: Instant, mut ready: impl FnMut(&mut App) -> bool) {
    loop {
        managed_source_app::poll(app, owner);
        if ready(app) { return; }
        assert!(Instant::now() < deadline,
            "physical App source lifecycle timed out: status={:?}, detail={:?}, terminal={}, source={}",
            app.agents.first().map(|run| &run.status),
            app.agents.first().and_then(|run|run.last_error.as_deref()),
            app.agents.first().is_some_and(|run|run.terminal.is_some()),
            app.managed_source_diagnostic(&app.agents[0].id));
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn run(input: &Input) {
    let (mut fixture, request) = qualified_request(input);
    let fence = request.fence().clone();
    let location = request.managed_run().unwrap();
    let marker = ManagedRunSource::Reserved { fence: fence.clone(),
        runnable_root: request.selection().root.root.clone() };
    let mut app = App::new();
    app.cwd = fixture.repo.clone();
    let mut row = crate::gitio::agent_from_run_record(&app.cwd,
        serde_json::json!({"id":fence.run_id,"task":"physical source App lifecycle",
            "mode":"execute","managedLocal":marker})).unwrap();
    row.status = AgentStatus::Running;
    row.managed_local = Some(marker);
    row.managed_store = Some(location);
    row.execution_profile = Some("offline-command-v1".into());
    app.agents.push(row);
    let supervisor = SourceOwnerLifetimeSupervisor::new_off_app().unwrap();
    app.configure_source_owner_supervisor(supervisor.clone()).unwrap();
    let writer = fixture.owner.as_ref().unwrap().source_journal_factory(&fence.owner).unwrap();
    app.queue_managed_source(0, request, input.native.clone(), writer, &fence.owner).unwrap();
    assert!(!crate::managed_app::accepts_input(&app, 0));
    drive(&mut app, &fence.owner, Instant::now() + Duration::from_secs(30), |app|
        crate::managed_app::accepts_input(app, 0)
            && app.agents[0].terminal.as_ref().is_some_and(|terminal|
                terminal.visible_lines_snapshot().join("\n").contains("source5-app-ready")));
    let idle = Instant::now() + Duration::from_millis(4000);
    while Instant::now() < idle {
        managed_source_app::poll(&mut app, &fence.owner);
        assert!(crate::managed_app::accepts_input(&app, 0));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(app.stop_agent_at(0));
    assert!(!crate::managed_app::accepts_input(&app, 0));
    assert!(app.shutdown_with_reason("physical test close before exit").is_err());
    assert!(fixture.owner.as_ref().unwrap().assert_current().is_ok());
    let mut checkpoint = None;
    drive(&mut app, &fence.owner, Instant::now() + Duration::from_secs(30), |app| {
        checkpoint = app.take_joined_source_checkpoint(&fence.run_id);
        checkpoint.is_some()
    });
    let checkpoint = checkpoint.unwrap();
    assert_eq!(checkpoint.assignment.fence, fence);
    let restored = checkpoint.managed_run.source_joined_checkpoint(&fence.attempt_id).unwrap().unwrap();
    assert_eq!(rudder_engine::protocol::canonical_json(&restored).unwrap(),
        rudder_engine::protocol::canonical_json(&checkpoint.record).unwrap());
    assert_eq!(app.agents[0].status, AgentStatus::Paused);
    assert!(app.agents[0].terminal.is_none());
    checkpoint.q.validate_data().unwrap();
    let store = rudder_engine::store::LocalStore::open(&checkpoint.store_root).unwrap();
    let plan = rudder_engine::delta::read_partial_tree(&store, &checkpoint.q.capture.retention).unwrap();
    assert_eq!(plan.base, input.graph.root);
    assert_eq!(plan.attempt, checkpoint.assignment.selection.attempt);
    let directories = rudder_engine::directory::LocalDirectoryStore::read_only(&store);
    let entry = rudder_engine::directory::PagedDirectory::new(&directories)
        .lookup(&plan.directory, b"runtime5-qualified.txt").unwrap().expect("captured guest mmap write");
    let rudder_engine::directory::EntryNode::File { object, logical_size, .. } = entry.node
        else { panic!("guest write kind differs") };
    let expected = b"Actual namespace5 mapped writer\n";
    assert_eq!(logical_size, expected.len() as u64);
    assert_eq!(rudder_engine::content::read_file_range(&store, &object, 0, expected.len() + 1).unwrap(), expected);
    let checkpoint_publication = input.checkpoint_transfer.as_ref().map(|transfer|
        checkpoint_transfer::publish(transfer, input, checkpoint.assignment.selection.clone(),
            &checkpoint.q, &checkpoint.store_root, &checkpoint.managed_run, fixture.directory())
            .unwrap_or_else(|error|panic!("App source checkpoint DATA publication failed: {error}")));
    drive(&mut app, &fence.owner, Instant::now() + Duration::from_secs(4), |app|
        app.ensure_managed_source_shutdown_ready().is_ok());
    app.shutdown_with_reason("physical source test completed").unwrap();
    let stopped = match supervisor.try_close_off_app() { Ok(stopped) => stopped,
        Err(_) => panic!("physical source supervisor retained unresolved ownership") };
    assert!(!stopped.join_worker_panicked);
    assert!(fixture.owner.as_ref().unwrap().assert_current().is_ok());
    let proof = serde_json::json!({"format_version":1,"kind":"private-runtime5-source-qualification",
        "status":"qualified","spec_version":5,"binding_version":4,"actual_https_source":true,
        "actual_backend_ready":true,"actual_uid1500_fuse_offline_namespace":true,
        "actual_app_owned_lifecycle":true,"actual_app_cancellation_gate":true,
        "actual_owner_bound_writer":true,"actual_off_app_owner_and_supervisor_join":true,
        "original_checkpoint_durable_before_retirement":true,
        "actual_stop_v4":true,"captured_mmap_bytes_verified":true,"actual_successful_helper_exit":true,
        "idle_source_freshness_window_ms":4000,"actual_helper_alive_after_idle_window":true,
        "actual_fresh_issuer_join":true,"actual_server_callbacks_settled":true,
        "record":checkpoint.record,"checkpoint_publication":checkpoint_publication,
        "accepted_jj_history":false,"model_execution":false,"default_rollout":false});
    write_new(&input.proof, &serde_json::to_vec(&proof).unwrap()).unwrap();
    fixture.owner.as_mut().unwrap().finish(true).unwrap();
}
