fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(Instant::now() < deadline, "source App lifecycle did not settle");
        thread::sleep(Duration::from_millis(5));
    }
}

fn source_app_fixture(request: &ManagedSourcePrepareRequest) -> App {
    let mut app = App::new();
    let location = request.managed_run().unwrap();
    app.cwd = location.store().binding().repository.clone();
    let marker = ManagedRunSource::Reserved { fence: request.fence().clone(),
        runnable_root: request.selection().root.root.clone() };
    let mut run = crate::gitio::agent_from_run_record(&app.cwd,
        serde_json::json!({"id":request.fence().run_id,"task":"owned source lifecycle",
            "mode":"execute","managedLocal":marker})).unwrap();
    run.managed_local = Some(marker);
    run.managed_store = Some(location);
    run.execution_profile = Some("offline-command-v1".into());
    run.status = AgentStatus::Running;
    app.agents.push(run);
    app
}

#[test]
fn cancelled_preparation_retains_app_and_engine_until_actual_join() {
    for replace_row in [false, true] {
        let (mut fixture, request) = source_prepare::tests::request();
        let fence = request.fence().clone();
        let mut app = source_app_fixture(&request);
        let supervisor = SourceOwnerLifetimeSupervisor::new_off_app().unwrap();
        app.configure_source_owner_supervisor(supervisor.clone()).unwrap();
        let (entered, wait) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let database = fixture.database.clone();
        // An actual writer held by the test's Engine owner, never a request path.
        let writer = Journal::open_with_busy_timeout(database, Duration::from_millis(20)).unwrap();
        let writer: SourceJournalFactory = Box::new(move || {
            entered.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(4)).unwrap();
            Ok(writer)
        });
        app.queue_managed_source(0, request, PathBuf::from("/usr/bin/true"), writer,
            &fixture.fence).unwrap();
        wait.recv_timeout(Duration::from_secs(4)).unwrap();
        assert!(!crate::managed_app::accepts_input(&app, 0));
        assert!(app.stop_agent_at(0));
        assert!(app.shutdown_with_reason("test close").is_err());
        assert!(fixture.owner.as_ref().unwrap().assert_current().is_ok());
        assert_eq!(app.managed_source.charged.load(Ordering::Acquire), 1);
        let marker = app.agents[0].managed_local.clone();
        if replace_row { app.agents[0].managed_local = None; }
        release.send(()).unwrap();
        if replace_row {
            until(|| {
                poll(&mut app, &fence.owner);
                app.managed_source.entries.get(&fence.run_id)
                    .is_some_and(|entry| entry.worker_join_pending && entry.worker.is_none())
            });
            assert!(app.ensure_managed_source_shutdown_ready().is_err());
            assert!(supervisor.status().iter().any(|slot| slot.occupied));
            app.agents[0].managed_local = marker;
        }
        until(|| {
            poll(&mut app, &fence.owner);
            app.ensure_managed_source_shutdown_ready().is_ok()
        });
        assert!(app.managed_source.entries.is_empty());
        assert_eq!(app.managed_source.charged.load(Ordering::Acquire), 0);
        assert_eq!(app.agents[0].status, AgentStatus::Paused);
        assert!(app.agents[0].terminal.is_none());
        assert!(fixture.owner.as_ref().unwrap().assert_current().is_ok());
        app.shutdown_with_reason("test close").unwrap();
        let stopped = match supervisor.try_close_off_app() {
            Ok(stopped) => stopped,
            Err(_) => panic!("settled source supervisor remained busy"),
        };
        assert!(!stopped.join_worker_panicked);
        fixture.owner.as_mut().unwrap().finish(true).unwrap();
    }
}
