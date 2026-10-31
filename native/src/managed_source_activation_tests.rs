// Included under managed_source_app::linux::tests. This is startup composition,
// not a namespace/backend/checkpoint or complete App-task qualification.
mod activation_regressions {
    use super::*;
    use crate::source_issuer::SourceIssuerFixture;
    use rudder_engine::execution::{handoff::receive_managed, startup_budget::StartupConstraint};
    use std::{io::{Read, Write}, os::unix::{fs::OpenOptionsExt, net::{UnixListener, UnixStream}}};

    const CHILD_ENV: &str = "RUDDER_SOURCE_APP_ACTIVATION_INPUT";
    const CHILD: &str = "managed_source_app::linux::tests::activation_regressions::delayed_descriptor_helper";
    const BOUND: Duration = Duration::from_secs(4);

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input { handoff:String, parent:u32, notifier:PathBuf, startup:StartupConstraint }

    fn private_file(path: &Path, bytes: &[u8], mode: u32) {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path).unwrap();
        file.write_all(bytes).unwrap(); file.sync_all().unwrap();
    }
    fn quoted(value: &str) -> String { format!("'{}'", value.replace('\'', "'\\''")) }
    fn byte(stream: &mut UnixStream) -> u8 {
        let mut value = [0]; stream.read_exact(&mut value).unwrap(); value[0]
    }

    struct Fixture {
        source:SourceIssuerFixture,
        prepared:Option<PreparedManagedSource>,
        app:App,
        supervisor:Arc<SourceOwnerLifetimeSupervisor>,
        fence:ManagedLaunchFence,
        commands:Receiver<Command>,
        gate:Option<UnixStream>,
        cleaned:bool,
    }
    impl Fixture {
        fn start() -> Self {
            let (mut source, request) = source_prepare::tests::request();
            let fence = request.fence().clone(); let selection = request.selection().clone();
            let mut app = source_app_fixture(&request);
            let prepared = source_prepare::prepare_owned(request, source.journal(), &AtomicBool::new(false)).unwrap();
            let handoff = ManagedExecutionHandoff::new(prepared.execution_spec(), prepared.binding().clone()).unwrap();
            let notifier = source.directory().join("activation.sock");
            let listener = UnixListener::bind(&notifier).unwrap(); listener.set_nonblocking(true).unwrap();
            let input = Input { handoff:handoff.helper_args()[4].clone(), parent:std::process::id(), notifier,
                startup:prepared.execution_spec().profile.as_ref().unwrap().startup.clone().unwrap() };
            let input_path = source.directory().join("activation-input.json");
            private_file(&input_path, &serde_json::to_vec(&input).unwrap(), 0o600);
            let native = source.directory().join("activation-helper");
            // Production Offered supplies the real helper argv. This private
            // executable enters only the isolated ignored test in this binary.
            let script = format!("#!/bin/sh\nexport {CHILD_ENV}={}\nexec {} --ignored --exact {CHILD} --nocapture --test-threads=1\n",
                quoted(input_path.to_str().unwrap()), quoted(std::env::current_exe().unwrap().to_str().unwrap()));
            private_file(&native, script.as_bytes(), 0o700);
            let supervisor = SourceOwnerLifetimeSupervisor::new_off_app().unwrap();
            app.configure_source_owner_supervisor(supervisor.clone()).unwrap();
            let supervision = supervisor.reserve(&fence).unwrap();
            let (send, commands) = mpsc::sync_channel(2);
            app.managed_source.entries.insert(fence.run_id.clone(), Entry {
                fence:fence.clone(), selection, assigned:None, location:prepared.managed_run.clone(), native,
                commands:send, cancelled:Arc::new(AtomicBool::new(false)), started:Instant::now(),
                stop_requested:false, recovery:false, retry_unused:false, worker:None,
                worker_join_pending:false, worker_settled:false, remove_after_join:false,
                recovery_owner:None, supervision, live:None, checkpoint:None,
            });
            let mut fixture = Self { source, prepared:Some(prepared), app, supervisor, fence,
                commands, gate:None, cleaned:false };
            let prepared = fixture.prepared.as_ref().unwrap();
            fixture.app.managed_source.send.try_send(Message { fence:fixture.fence.clone(),
                event:Event::Offered { assignment:prepared.assignment().clone(), handoff, issuer:prepared.issuer().unwrap() } })
                .unwrap_or_else(|_|panic!("fixture Offered admission failed"));
            fixture.poll();
            let end = Instant::now() + BOUND;
            let mut gate = loop {
                match listener.accept() {
                    Ok((gate,_)) => break gate,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < end, "delayed helper did not connect");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => panic!("delayed helper socket failed"),
                }
            };
            gate.set_read_timeout(Some(BOUND)).unwrap(); gate.set_write_timeout(Some(BOUND)).unwrap();
            assert_eq!(byte(&mut gate), b'P'); fixture.gate = Some(gate); fixture
        }
        fn poll(&mut self) { poll(&mut self.app, &self.fence.owner); }
        fn entry(&self) -> &Entry { self.app.managed_source.entries.get(&self.fence.run_id).unwrap() }
        fn live(&self) -> &Live { self.entry().live.as_ref().expect("production Offered created actual live custody") }
        fn reap(&mut self) -> bool {
            let end = Instant::now() + BOUND;
            let Some(pane) = self.app.agents[0].terminal.as_mut() else { return true };
            loop {
                pane.drain_output();
                if let Ok(Some(status)) = pane.try_wait_after_output() { return status.success(); }
                assert!(Instant::now() < end, "delayed helper did not actually reap");
                thread::sleep(Duration::from_millis(2));
            }
        }
        fn cleanup(&mut self) -> bool {
            self.gate.take(); // Release a waiting child on assertion unwind too.
            if let Some(pane) = self.app.agents[0].terminal.as_mut() {
                if pane.child_process_id().is_some() { pane.request_termination(); }
                let end = Instant::now() + BOUND;
                loop {
                    pane.drain_output();
                    match pane.try_wait_after_output() {
                        Ok(Some(_)) => break,
                        Err(_) => return false,
                        _ if Instant::now() >= end => return false,
                        _ => thread::sleep(Duration::from_millis(2)),
                    }
                }
            }
            self.app.agents[0].terminal.take();
            if let Some(entry) = self.app.managed_source.entries.get_mut(&self.fence.run_id) { entry.live.take(); }
            if let Some(prepared) = self.prepared.as_mut() {
                if source_prepare::tests::retire_reaped_startup_fixture(prepared, self.source.journal()).is_err() { return false; }
            }
            self.prepared.take();
            // No source-owner worker was started: this isolated fixture owns
            // and actually reaps/retires the helper/preparation above. No
            // Settled actor or JoinedSourceShutdown is fabricated for release.
            if let Some(entry) = self.app.managed_source.entries.remove(&self.fence.run_id) {
                entry.supervision.cancel_before_effects();
            }
            let end = Instant::now() + BOUND;
            while self.supervisor.status().iter().any(|slot|slot.occupied) {
                if Instant::now() >= end { return false; }
                thread::sleep(Duration::from_millis(2));
            }
            self.cleaned = match self.supervisor.clone().try_close_off_app() {
                Ok(proof) => !proof.join_worker_panicked, Err(_) => false,
            };
            self.cleaned
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { if !self.cleaned { let _ = self.cleanup(); } } }

    #[test]
    fn app_waits_for_actual_descriptor_transfer_then_activates_the_original_witness() {
        let mut fixture = Fixture::start();
        let original = fixture.live().control.helper_process_witness();
        let startup = fixture.prepared.as_ref().unwrap().execution_spec().profile.as_ref().unwrap().startup.clone();
        let budget = fixture.prepared.as_ref().unwrap().execution_spec().startup_budget().unwrap().unwrap();
        let before = budget.remaining().unwrap();
        let until = Instant::now() + Duration::from_millis(100);
        loop {
            fixture.poll();
            assert!(!fixture.live().handoff_transferred && !fixture.live().activation_requested);
            assert!(!fixture.live().activated && !fixture.entry().recovery);
            assert!(fixture.live().handoff.try_result().is_none());
            assert!(original.check_current().is_err()); // Actually PENDING, not a synthetic flag.
            assert!(matches!(fixture.commands.try_recv(), Err(mpsc::TryRecvError::Empty)));
            if Instant::now() >= until { break; }
            thread::sleep(Duration::from_millis(2));
        }
        fixture.gate.as_mut().unwrap().write_all(b"1").unwrap();
        assert_eq!(byte(fixture.gate.as_mut().unwrap()), b'D');
        let mut admitted = None;
        super::until(|| {
            fixture.poll();
            match fixture.commands.try_recv() {
                Ok(Command::Activate(witness)) => { admitted = Some(witness); true },
                Err(mpsc::TryRecvError::Empty) => false,
                _ => panic!("descriptor transfer produced a different App command"),
            }
        });
        let admitted = admitted.unwrap();
        assert!(fixture.live().handoff_transferred && fixture.live().activation_requested);
        assert!(Arc::ptr_eq(&original, &admitted));
        admitted.verify_binding(fixture.prepared.as_ref().unwrap().binding()).unwrap();
        let prepared = fixture.prepared.as_mut().unwrap();
        prepared.adopt(); prepared.activate_helper(admitted).unwrap();
        fixture.app.managed_source.send.try_send(Message { fence:fixture.fence.clone(), event:Event::Activated(Ok(())) })
            .unwrap_or_else(|_|panic!("fixture activation acknowledgment admission failed"));
        for _ in 0..4 { fixture.poll(); }
        assert!(fixture.live().activated && !fixture.entry().recovery);
        assert!(matches!(fixture.commands.try_recv(), Err(mpsc::TryRecvError::Empty))); // Exactly one activation.
        assert_eq!(fixture.prepared.as_ref().unwrap().execution_spec().profile.as_ref().unwrap().startup, startup);
        assert!(budget.remaining().unwrap() < before); // Delay spent the original absolute budget.
        fixture.gate.as_mut().unwrap().write_all(b"2").unwrap();
        assert!(fixture.reap()); assert!(original.check_current().is_err());
        assert!(fixture.cleanup());
    }

    #[test]
    fn app_never_activates_if_the_actual_pending_helper_exits_before_transfer() {
        let mut fixture = Fixture::start();
        let original = fixture.live().control.helper_process_witness();
        assert!(original.check_current().is_err());
        fixture.gate.as_mut().unwrap().write_all(b"0").unwrap();
        assert!(fixture.reap());
        super::until(|| { fixture.poll(); fixture.entry().recovery });
        assert!(!fixture.live().handoff_transferred && !fixture.live().activation_requested);
        assert!(original.check_current().is_err());
        while let Ok(command) = fixture.commands.try_recv() {
            assert!(!matches!(command, Command::Activate(_)), "failed handoff activated a source");
        }
        assert!(fixture.cleanup());
    }

    #[test]
    #[ignore = "private PTY child invoked only by the delayed App handoff fixture"]
    fn delayed_descriptor_helper() {
        let path = std::env::var_os(CHILD_ENV).expect("private activation parent required");
        let input:Input = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let mut gate = UnixStream::connect(&input.notifier).unwrap();
        gate.set_read_timeout(Some(BOUND)).unwrap(); gate.set_write_timeout(Some(BOUND)).unwrap();
        gate.write_all(b"P").unwrap();
        match byte(&mut gate) { b'0' => return, b'1' => {}, _ => panic!("invalid private gate") }
        let (spec, control) = receive_managed(&input.handoff, input.parent).unwrap();
        assert_eq!(spec.format_version, 5); assert_eq!(control.binding.format_version, 4);
        assert_eq!(spec.profile.as_ref().unwrap().startup.as_ref(), Some(&input.startup));
        assert_eq!(control.binding.profile.as_ref().unwrap().startup.as_ref(), Some(&input.startup));
        gate.write_all(b"D").unwrap(); assert_eq!(byte(&mut gate), b'2'); drop(control);
    }
}
