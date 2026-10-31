//! Explicit retained-local launches. App owns reservation, result adoption and
//! PTYs; bounded workers own all materialization, credentials, grant and bind IO.
use super::*;
use crate::managed_launch::{ManagedLaunchFence, ManagedLocalLaunchInputs, ManagedRunSource};
use rudder_engine::journal::{Journal, OwnerFence};

#[cfg(target_os = "linux")]
mod local {
    use super::*;
    use crate::{agent_broker::{AgentBroker, BrokerConfig}, managed_launch::{self, PreparedManagedLaunch}};
    use rudder_engine::{execution::{handoff::{HandoffReceipt, ManagedExecutionHandoff, ManagedExecutionHandle, ManagedExecutionEvent, ValidatedManagedShutdown}, StopAndCheckpoint}, paths::FabricPaths};
    use std::sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Arc, mpsc::{self, Receiver, SyncSender}};
    use std::collections::BTreeMap;

    const MAX_PREPARATIONS: usize = 2;
    const PREPARATION_DEADLINE: Duration = Duration::from_secs(120);
    const STOP_DEADLINE: Duration = Duration::from_secs(30);
    fn now_ms() -> Result<i64> {
        Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis().try_into()?)
    }

    /// A timed-out/cancelled job retains its admission until actual settlement.
    /// The bounded result channel also retains admission if App falls behind.
    struct WorkerQueue<T> { active: Arc<AtomicUsize>, send: SyncSender<T>, receive: Receiver<T> }
    struct WorkerPermit(Arc<AtomicUsize>);
    impl Drop for WorkerPermit { fn drop(&mut self) { self.0.fetch_sub(1, Ordering::AcqRel); } }
    impl<T: Send + 'static> WorkerQueue<T> {
        fn new() -> Self { let (send, receive) = mpsc::sync_channel(MAX_PREPARATIONS);
            Self { active: Arc::new(AtomicUsize::new(0)), send, receive } }
        fn admit(&self) -> Result<WorkerPermit> {
            self.active.fetch_update(Ordering::AcqRel, Ordering::Acquire,
                |active| (active < MAX_PREPARATIONS).then_some(active + 1))
                .map_err(|_| anyhow::anyhow!("managed launch preparation is busy"))?;
            Ok(WorkerPermit(self.active.clone()))
        }
        fn spawn(&self, permit: WorkerPermit, waker: Option<PtyOutputWaker>, work: impl FnOnce(SyncSender<T>) -> T + Send + 'static,
            on_panic: impl FnOnce() -> T + Send + 'static) -> Result<()> {
            let send = self.send.clone();
            thread::Builder::new().name("rudder-managed-prepare".into()).spawn(move || {
                let _permit = permit;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||work(send.clone())))
                    .unwrap_or_else(|_|on_panic());
                let _ = send.send(result);
                if let Some(waker) = waker { waker(); }
            })?;
            Ok(())
        }
    }

    struct Pending { fence: ManagedLaunchFence, cancelled: Arc<AtomicBool>, started: Instant }
    struct PreparedResult { prepared: PreparedManagedLaunch, broker: AgentBroker, projection_hash: [u8; 32],
        handoff: ManagedExecutionHandoff, native_program: PathBuf, launch_json: String }
    type BackendProcess = Arc<rudder_engine::execution::handoff::BackendProcessWitness>;
    enum WorkerStep { Workspace { prepared: PreparedManagedLaunch, proceed: SyncSender<PreparedManagedLaunch> }, Ready(PreparedResult), Reports(Result<CandidateReports>),
        Authenticated { process:BackendProcess, result:std::result::Result<(),agent_link::ManagedCodexAuthError> } }
    struct WorkerResult { fence: ManagedLaunchFence, result: Result<WorkerStep> }
    struct PendingAuth { process:BackendProcess, cancelled:Arc<AtomicBool>, deadline:Instant, running:bool }
    struct Live { prepared: PreparedManagedLaunch, handoff: HandoffReceipt, control: ManagedExecutionHandle,
        receipt: Option<ValidatedManagedShutdown>, stop: Option<(StopAndCheckpoint, Instant)>, uncertain: bool,
        orphan_reported: bool, backend_ready: bool, auth:Option<PendingAuth>, report_pending: bool, last_report: Option<Instant>, report_digest: Option<[u8;32]> }
    impl Drop for Live { fn drop(&mut self) { if let Some(auth) = &self.auth { auth.cancelled.store(true, Ordering::Release); } } }
    struct CandidateReports { summary: Option<String>, signal: Option<signals::SignalState>, digest: [u8;32] }

    fn read_reports(paths: &managed_launch::ManagedReportMapping) -> Result<CandidateReports> {
        use std::{fs::{File, OpenOptions}, io::Read, os::{fd::{AsRawFd, FromRawFd}, unix::fs::OpenOptionsExt}, ffi::CStr};
        use sha2::{Digest, Sha256};
        fn open_at(parent: &File, name: &CStr, flags: i32) -> Result<Option<File>> {
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | flags) };
            if fd < 0 { let error = io::Error::last_os_error(); return if error.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(error.into()) }; }
            Ok(Some(unsafe { File::from_raw_fd(fd) }))
        }
        fn read(file: Option<File>) -> Result<Option<Vec<u8>>> {
            let Some(file) = file else { return Ok(None); };
            let metadata = file.metadata()?;
            anyhow::ensure!(metadata.is_file() && metadata.len() <= 64 * 1024, "managed report exceeds regular file bound");
            let mut bytes = Vec::new(); file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 64 * 1024, "managed report exceeds bound"); Ok(Some(bytes))
        }
        let directory = paths.host_done_file.parent().context("managed report directory absent")?;
        anyhow::ensure!(paths.host_signal_file.parent() == Some(directory)
            && directory.file_name().is_some_and(|name|name == ".rudder-reports")
            && paths.host_done_file.file_name().is_some_and(|name|name == "done.json")
            && paths.host_signal_file.file_name().is_some_and(|name|name == "signal.json"), "managed report mapping changed");
        let home = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.parent().context("managed execution home absent")?)?;
        // The guest may rename entries inside its home. Traverse every writable
        // component by held dirfd and refuse symlinks, including the parent dir.
        let Some(directory) = open_at(&home, c".rudder-reports", libc::O_DIRECTORY)? else {
            return Ok(CandidateReports { summary: None, signal: None, digest: Sha256::digest([]).into() });
        };
        let done = read(open_at(&directory, c"done.json", 0)?)?;
        let signal = read(open_at(&directory, c"signal.json", 0)?)?;
        let digest = Sha256::digest(rudder_engine::protocol::canonical_json(&(done.as_deref(), signal.as_deref()))?).into();
        let summary = done.and_then(|bytes|serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value|value.get("summary").and_then(serde_json::Value::as_str).map(str::to_owned))
            .map(|value|value.chars().filter(|ch|!ch.is_control() || matches!(*ch, '\n' | '\t')).take(4096).collect::<String>())
            .map(|value|value.trim().to_owned()).filter(|value|!value.is_empty());
        let signal = signal.and_then(|bytes|String::from_utf8(bytes).ok()).and_then(|body|signals::parse_signal_state(&body));
        Ok(CandidateReports { summary, signal, digest })
    }
    fn apply_reports(run: &mut AgentRun, reports: CandidateReports, attention_allowed: bool) {
        if let Some(summary) = reports.summary { run.done_summary = Some(summary); }
        if !attention_allowed { return; }
        match reports.signal {
            Some(signals::SignalState::Permission) => { run.wait_signal = Some(WaitSignal::Permission); run.needs_permission = true; run.needs_user_input = false; }
            Some(signals::SignalState::Input) => { run.wait_signal = Some(WaitSignal::Input); run.needs_user_input = true; }
            Some(signals::SignalState::Working) => { run.wait_signal = None; run.needs_permission = false; run.needs_user_input = false; }
            // Writable reports cannot complete/fail a model turn or certify a
            // delivered ACK, deployment, readiness or retained checkpoint.
            _ => {}
        }
    }

    pub(crate) struct ManagedAppState { queue: WorkerQueue<WorkerResult>, pending: BTreeMap<String, Pending>, live: BTreeMap<String, Live> }
    impl Default for ManagedAppState { fn default() -> Self { Self { queue: WorkerQueue::new(), pending: BTreeMap::new(), live: BTreeMap::new() } } }
    impl Drop for ManagedAppState { fn drop(&mut self) { for pending in self.pending.values() { pending.cancelled.store(true, Ordering::Release); } } }

    fn still_pending(app: &App, fence: &ManagedLaunchFence, owner: &OwnerFence) -> bool {
        owner == &fence.owner && app.agents.iter().any(|run| run.id == fence.run_id
            && run.terminal.is_none() && run.status == AgentStatus::Running
            && run.managed_local.as_ref().and_then(ManagedRunSource::fence) == Some(fence))
    }
    fn matches_workspace(source: Option<&ManagedRunSource>, assignment: &managed_launch::WorkspaceAssignment, reserved: bool) -> bool {
        match source {
            Some(ManagedRunSource::Reserved { fence, runnable_root }) if reserved => fence == &assignment.fence && runnable_root == &assignment.runnable_root,
            Some(ManagedRunSource::Assigned { assignment: expected }) => expected.as_ref() == assignment,
            _ => false,
        }
    }

    impl App {
        /// Explicit opt-in only. The caller supplies an existing stopped row and
        /// a retained local selection. No default/profile/cwd fallback exists.
        pub(crate) fn start_managed_local(&mut self, index: usize, inputs: ManagedLocalLaunchInputs,
            native_program: PathBuf, journal: &mut Journal, owner: &OwnerFence) -> Result<ManagedLaunchFence> {
            let run = self.agents.get(index).context("managed launch row is absent")?;
            anyhow::ensure!(run.terminal.is_none() && !matches!(run.status, AgentStatus::Running | AgentStatus::Merged | AgentStatus::Migrated)
                && run.managed_local.is_none() && run.session_id.is_none() && !run.plain_process
                && !run.is_main() && !run.is_orchestrator() && !is_cloud_owned_agent(run)
                && run.workspace_path.is_none() && run.jj_change_id.is_none(),
                "managed local launch requires a new explicitly selected row");
            anyhow::ensure!(run.backend == inputs.backend.backend() && native_program.is_absolute(), "managed backend/program selection differs from the row");
            anyhow::ensure!(inputs.journal == FabricPaths::new(&inputs.home).journal,
                "managed launch journal differs from its explicit identity home");
            anyhow::ensure!(journal.database_path()? == inputs.journal,
                "managed launch worker journal differs from App ownership");
            anyhow::ensure!(inputs.managed_store.binding().home == inputs.home
                && inputs.managed_store.binding().journal == inputs.journal
                && inputs.managed_store.binding().repository == self.cwd
                && inputs.managed_store.binding().project_id == owner.project_id
                && journal.managed_project(&owner.project_id)?.as_ref() == Some(inputs.managed_store.receipt())
                && inputs.preparation_parent == inputs.managed_store.binding().directory.join("preparations"),
                "managed launch requires its exact explicitly prepared private project store");
            let location = inputs.managed_store.run(&run.id)?;
            let permit = self.managed_local.queue.admit()?;
            let attempt_id = uuid::Uuid::new_v4().to_string();
            let request_id = uuid::Uuid::new_v4().to_string();
            let operation_id = uuid::Uuid::new_v4().to_string();
            let view_id = uuid::Uuid::new_v4().to_string();
            let run_id = run.id.clone();
            let epoch = journal.observe_run_attempt(owner, &run_id, &attempt_id, now_ms()?)?;
            let fence = ManagedLaunchFence { owner: owner.clone(), run_id: run_id.clone(), run_epoch: epoch, attempt_id, request_id };
            let source = ManagedRunSource::Reserved { fence: fence.clone(), runnable_root: inputs.runnable_root.clone() };
            source.validate(&run_id)?;
            let run = &mut self.agents[index];
            run.managed_local = Some(source); run.status = AgentStatus::Running;
            run.managed_store = Some(location.clone());
            run.execution_profile = Some(inputs.backend.execution_profile().into());
            run.completed_at = None; run.last_error = Some("preparing managed local source".into());
            // This marker contains no credentials and must precede worker effects.
            // Managed persistence performs no Git/history queries.
            if let Err(error) = save_native_run_record(&self.cwd, run) {
                run.status = AgentStatus::Orphaned;
                return Err(error.context("persist managed reservation before preparation"));
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            self.managed_local.pending.insert(run_id.clone(), Pending { fence: fence.clone(), cancelled: cancelled.clone(), started: Instant::now() });
            let request = inputs.bind(fence.clone(), operation_id, view_id);
            let worker_fence = fence.clone();
            let reservation = self.agents[index].managed_local.clone().unwrap();
            let reservation_projection = serde_json::json!({"id":run_id,"task":self.agents[index].task,
                "backend":self.agents[index].backend.as_str(),"model":self.agents[index].model,
                "status":"orphaned","mode":"execute","executionProfile":self.agents[index].execution_profile});
            let stage_waker = self.pty_output_waker.clone();
            let panic_fence = worker_fence.clone();
            let spawn = self.managed_local.queue.spawn(permit, self.pty_output_waker.clone(), move |events| {
                let result = (|| -> Result<PreparedResult> {
                    let home = request.home.clone(); let journal_path = request.journal.clone(); let backend = request.backend.backend();
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before durable reservation");
                    location.persist_reservation(&reservation)?;
                    location.write_projection(&reservation, &reservation_projection)?;
                    let prepared = managed_launch::prepare_local(request, cancelled.clone())?;
                    // Immutable private evidence precedes the App's assignment
                    // acknowledgment and every scoped grant/publication.
                    location.persist_assignment(&prepared.assignment)?;
                    let (proceed, receive) = mpsc::sync_channel(1);
                    events.send(WorkerResult { fence: worker_fence.clone(), result: Ok(WorkerStep::Workspace { prepared, proceed }) })
                        .map_err(|_|anyhow::anyhow!("managed App retired before workspace assignment"))?;
                    if let Some(waker) = &stage_waker { waker(); }
                    let prepared = receive.recv_timeout(PREPARATION_DEADLINE).context("managed assignment adoption did not settle")?;
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before grant");
                    let mut unused_command = TerminalCommand::with_args("", Vec::<String>::new());
                    let projection = fabric_runtime::prepare_agent_tools_for_attempt(&mut unused_command, backend, &home,
                        &worker_fence, Some(&prepared.assignment))?;
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before broker bind");
                    let broker = AgentBroker::spawn(BrokerConfig { journal_path: journal_path.clone(),
                        identity_file: projection.identity_file, socket_path: projection.broker_socket })?;
                    let handoff = prepared.prepare_handoff()?;
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before result");
                    worker_fence.check_current(&Journal::open(&journal_path)?)?;
                    let launch_json = serde_json::to_string(&prepared.link_launch())?;
                    Ok(PreparedResult { prepared, broker, projection_hash: projection.projection_hash, handoff, native_program, launch_json })
                })();
                WorkerResult { fence: worker_fence, result: result.map(WorkerStep::Ready) }
            }, move ||WorkerResult { fence:panic_fence, result:Err(anyhow::anyhow!("managed preparation worker failed without a result")) });
            if let Err(error) = spawn {
                self.managed_local.pending.remove(&run_id);
                let run = &mut self.agents[index]; run.status = AgentStatus::Orphaned;
                run.last_error = Some("managed preparation worker could not start; reservation needs recovery".into());
                let _ = save_native_run_record(&self.cwd, run);
                return Err(error);
            }
            self.dirty = true;
            Ok(fence)
        }
    }

    pub(crate) fn poll(app: &mut App, journal: &mut Journal, owner: &OwnerFence) -> Result<()> {
        let mut state = std::mem::take(&mut app.managed_local);
        let result = state.poll(app, journal, owner);
        app.managed_local = state; result
    }
    impl ManagedAppState {
        fn poll(&mut self, app: &mut App, journal: &mut Journal, owner: &OwnerFence) -> Result<()> {
            for pending in self.pending.values() {
                if pending.started.elapsed() >= PREPARATION_DEADLINE || !still_pending(app, &pending.fence, owner)
                    || pending.fence.check_current(journal).is_err() {
                    pending.cancelled.store(true, Ordering::Release);
                    if let Some(run) = app.agents.iter_mut().find(|run|run.id == pending.fence.run_id
                        && run.managed_local.as_ref().and_then(ManagedRunSource::fence) == Some(&pending.fence)
                        && run.status == AgentStatus::Running) {
                        run.status = AgentStatus::Orphaned; run.last_error = Some("managed preparation cancelled/stale/timed out; no replacement launched".into());
                        let _ = save_native_run_record(&app.cwd, run); app.dirty = true;
                    }
                }
            }
            while let Ok(result) = self.queue.receive.try_recv() {
                if let Ok(WorkerStep::Authenticated { process, result:authenticated }) = result.result {
                    if let Some(live) = self.live.get_mut(&result.fence.run_id).filter(|live|live.prepared.assignment.fence == result.fence) {
                        if live.stop.is_some() || live.uncertain { continue; }
                        let current = !live.uncertain && live.stop.is_none() && result.fence.owner == *owner
                            && result.fence.check_current(journal).is_ok() && live.auth.as_ref().is_some_and(|auth|
                                auth.running && Arc::ptr_eq(&auth.process, &process) && !auth.cancelled.load(Ordering::Acquire) && Instant::now() < auth.deadline);
                        if current && authenticated.is_ok() {
                            if let Some(index) = app.agents.iter().position(|run|run.id == result.fence.run_id
                                && matches_workspace(run.managed_local.as_ref(), &live.prepared.assignment, false)) {
                                let expected = app.agents[index].session_id.clone();
                                let attached = app.agents[index].terminal.as_mut().context("managed authenticated terminal absent").and_then(|terminal|
                                    app.fabric_runtime.attach_managed_backend(owner, &live.prepared.assignment, terminal, live.prepared.link_launch(),
                                        live.prepared.transport.guest_cwd.clone(), expected, process));
                                if attached.is_ok() && live.control.backend_authenticated().is_ok() { live.backend_ready = true; live.auth = None; }
                                else { live.uncertain = true; }
                            } else { live.uncertain = true; }
                        } else { live.uncertain = true; }
                    }
                    continue;
                }
                if let Ok(WorkerStep::Reports(reports)) = result.result {
                    if let Some(live) = self.live.get_mut(&result.fence.run_id).filter(|live|live.prepared.assignment.fence == result.fence) { live.report_pending = false; }
                    if result.fence.owner == *owner && result.fence.check_current(journal).is_ok() {
                        if let (Ok(reports), Some(run)) = (reports, app.agents.iter_mut().find(|run|run.id == result.fence.run_id
                            && run.managed_local.as_ref().and_then(ManagedRunSource::fence) == Some(&result.fence))) {
                            let changed = self.live.get(&result.fence.run_id).is_none_or(|live|live.report_digest != Some(reports.digest));
                            if changed {
                                if let Some(live) = self.live.get_mut(&result.fence.run_id) { live.report_digest = Some(reports.digest); }
                                let attention_allowed = run.terminal.is_some() && self.live.get(&result.fence.run_id)
                                    .is_some_and(|live|live.backend_ready && live.stop.is_none() && !live.uncertain);
                                apply_reports(run, reports, attention_allowed); let _ = save_native_run_record(&app.cwd, run); app.dirty = true;
                            }
                        }
                    }
                    continue;
                }
                let workspace_stage = matches!(&result.result, Ok(WorkerStep::Workspace { .. }));
                let exact = self.pending.get(&result.fence.run_id).is_some_and(|pending|pending.fence == result.fence
                    && !pending.cancelled.load(Ordering::Acquire));
                if !workspace_stage && self.pending.get(&result.fence.run_id).is_some_and(|pending|pending.fence == result.fence) {
                    self.pending.remove(&result.fence.run_id);
                }
                if !exact || !still_pending(app, &result.fence, owner) || result.fence.check_current(journal).is_err() { continue; }
                let index = app.agents.iter().position(|run|run.id == result.fence.run_id).unwrap();
                match result.result {
                    Ok(WorkerStep::Workspace { prepared, proceed }) => {
                        if journal.database_path()? != prepared.identity_journal || prepared.assignment.validate(&result.fence).is_err()
                            || app.agents[index].managed_store.as_ref() != Some(&prepared.managed_run)
                            || !matches_workspace(app.agents[index].managed_local.as_ref(), &prepared.assignment, true) {
                            orphan(&app.cwd, &mut app.agents[index], "managed workspace result changed its reserved source/journal; no capability granted");
                            continue;
                        }
                        app.agents[index].managed_local = Some(ManagedRunSource::Assigned { assignment: Box::new(prepared.assignment.clone()) });
                        if save_native_run_record(&app.cwd, &app.agents[index]).is_err() {
                            if let Some(pending) = self.pending.get(&result.fence.run_id) { pending.cancelled.store(true, Ordering::Release); }
                            orphan(&app.cwd, &mut app.agents[index], "managed assignment persistence failed; no capability was granted");
                        } else if proceed.try_send(prepared).is_err() {
                            orphan(&app.cwd, &mut app.agents[index], "managed assignment continuation retired; no helper was spawned");
                        }
                    }
                    Ok(WorkerStep::Ready(mut ready)) => {
                        if journal.database_path()? != ready.prepared.identity_journal || ready.prepared.assignment.validate(&result.fence).is_err()
                            || app.agents[index].managed_store.as_ref() != Some(&ready.prepared.managed_run)
                            || !matches_workspace(app.agents[index].managed_local.as_ref(), &ready.prepared.assignment, false) {
                            orphan(&app.cwd, &mut app.agents[index], "managed helper result changed its assigned scope/journal; no helper spawned");
                            continue;
                        }
                        let assignment = ready.prepared.assignment.clone();
                        app.agents[index].managed_local = Some(ManagedRunSource::Assigned { assignment: Box::new(assignment.clone()) });
                        if save_native_run_record(&app.cwd, &app.agents[index]).is_err() {
                            orphan(&app.cwd, &mut app.agents[index], "managed assignment could not be persisted; helper was not spawned");
                            continue;
                        }
                        // A failed spawn/activation may still have created a child.
                        // From this point Drop cannot unpin/remove the live source.
                        ready.prepared.adopt();
                        let options = TerminalPaneOptions { size: app.agents[index].terminal_size.unwrap_or_default(),
                            cwd: None, ..TerminalPaneOptions::default() };
                        match TerminalPane::spawn_managed(ready.native_program.display().to_string(), ready.handoff,
                            &assignment.fence.attempt_id, &ready.prepared.execution_profile, Some(&ready.launch_json), options) {
                            Ok((mut terminal, handoff, control)) => {
                                App::attach_output_waker(&app.pty_output_waker, &terminal);
                                let adoption = app.fabric_runtime.adopt_managed_broker(owner, &assignment, &ready.prepared.identity_home,
                                    &mut terminal, ready.broker, ready.projection_hash);
                                if adoption.is_err() {
                                    terminal.request_termination();
                                    orphan(&app.cwd, &mut app.agents[index], "managed broker adoption failed after helper spawn; source retained for recovery");
                                } else {
                                    app.agents[index].terminal = Some(terminal);
                                    app.agents[index].last_error = None;
                                    self.live.insert(assignment.fence.run_id.clone(), Live { prepared: ready.prepared,
                                        handoff, control, receipt: None, stop: None, uncertain: false, orphan_reported: false,
                                        backend_ready: false, auth:None, report_pending: false, last_report: None, report_digest: None });
                                    let _ = save_native_run_record(&app.cwd, &app.agents[index]);
                                }
                            }
                            Err(_) => orphan(&app.cwd, &mut app.agents[index], "managed helper spawn/activation failed; source retained for recovery"),
                        }
                    }
                    Err(_) => orphan(&app.cwd, &mut app.agents[index], "managed local preparation failed; no checkout fallback was launched"),
                    Ok(WorkerStep::Reports(_)) => unreachable!(),
                    Ok(WorkerStep::Authenticated { .. }) => unreachable!(),
                }
                app.dirty = true;
            }
            let ids: Vec<_> = self.live.keys().cloned().collect();
            for id in ids {
                let pending_auth = self.live.values().filter(|live|live.auth.is_some()).count();
                let live = self.live.get_mut(&id).unwrap();
                if pending_auth == 0 && !live.report_pending && live.last_report.is_none_or(|last|last.elapsed() >= Duration::from_millis(500)) {
                    if let Ok(permit) = self.queue.admit() {
                        let paths = live.prepared.reports.clone(); let fence = live.prepared.assignment.fence.clone();
                        let panic_fence = fence.clone();
                        if self.queue.spawn(permit, app.pty_output_waker.clone(), move |_|WorkerResult { fence, result: Ok(WorkerStep::Reports(read_reports(&paths))) },
                            move ||WorkerResult { fence:panic_fence, result:Ok(WorkerStep::Reports(Err(anyhow::anyhow!("managed report worker failed")))) }).is_ok() {
                            live.report_pending = true; live.last_report = Some(Instant::now());
                        }
                    }
                }
                let assignment = &live.prepared.assignment;
                let Some(index) = app.agents.iter().position(|run|run.id == id
                    && matches_workspace(run.managed_local.as_ref(), assignment, false)) else {
                    app.fabric_runtime.close_managed_assignment(assignment);
                    if let Some(run) = app.agents.iter_mut().find(|run|run.id == id && run.terminal.as_ref()
                        .is_some_and(|terminal|terminal.instance_id() == assignment.fence.attempt_id)) {
                        if let Some(terminal) = run.terminal.as_mut() { terminal.request_termination(); }
                        run.terminal = None;
                        orphan(&app.cwd, run, "managed helper assignment was replaced; old source retained without checkpoint acknowledgment");
                    }
                    self.live.remove(&id); continue;
                };
                if assignment.fence.owner != *owner || assignment.fence.check_current(journal).is_err() {
                    live.uncertain = true;
                }
                if let Some(Err(_)) = live.handoff.try_result() { live.uncertain = true; }
                while let Some(event) = live.control.try_event() {
                    match event {
                        ManagedExecutionEvent::BackendReady { process } if !live.uncertain && live.stop.is_none() => {
                            if live.prepared.execution_profile == "codex-app-server" {
                                if process.authenticated_start_required() && live.auth.is_none() && pending_auth < 8 {
                                    live.auth = Some(PendingAuth { process, cancelled:Arc::new(AtomicBool::new(false)),
                                        deadline:Instant::now()+Duration::from_secs(20), running:false });
                                } else { live.uncertain = true; }
                                continue;
                            }
                            let expected = app.agents[index].session_id.clone();
                            if let Some(terminal) = app.agents[index].terminal.as_mut() {
                                if app.fabric_runtime.attach_managed_backend(owner, assignment, terminal, live.prepared.link_launch(),
                                    live.prepared.transport.guest_cwd.clone(), expected, process).is_err() { live.uncertain = true; }
                                else { live.backend_ready = true; }
                            } else { live.uncertain = true; }
                        }
                        ManagedExecutionEvent::Shutdown(receipt) => {
                            if live.stop.as_ref().is_some_and(|(request, _)| request == receipt.request()) {
                                live.receipt = Some(receipt);
                            } else { live.uncertain = true; }
                        }
                        ManagedExecutionEvent::BackendReady { .. } => {}
                        ManagedExecutionEvent::Uncertain => live.uncertain = true,
                    }
                }
                if live.stop.as_ref().is_some_and(|(_, at)| at.elapsed() > STOP_DEADLINE) { live.uncertain = true; }
                if live.stop.is_none() && live.auth.as_ref().is_some_and(|auth|Instant::now() >= auth.deadline) { live.uncertain = true; }
                if live.uncertain { if let Some(auth) = &live.auth { auth.cancelled.store(true, Ordering::Release); } }
                let run = &mut app.agents[index];
                let exit = if let Some(terminal) = run.terminal.as_mut() {
                    if !terminal.drain_output().is_empty() { run.last_output_at = Instant::now(); app.dirty = true; }
                    let _ = terminal.take_notifications();
                    if live.uncertain && !live.orphan_reported { terminal.request_termination(); }
                    terminal.try_wait_after_output()
                } else { Ok(None) };
                if live.uncertain {
                    let first = !live.orphan_reported;
                    let ended = !matches!(exit, Ok(None)) || run.terminal.is_none();
                    if first {
                        app.fabric_runtime.close_managed_assignment(assignment);
                        run.status = AgentStatus::Orphaned; run.completed_at = None;
                        run.last_error = Some("managed helper outcome is uncertain; no checkpoint acknowledged".into());
                        live.orphan_reported = true;
                    }
                    if ended {
                        run.terminal = None; self.live.remove(&id);
                    }
                    if first || ended { let _ = save_native_run_record(&app.cwd, run); app.dirty = true; }
                } else if let Ok(Some(_)) = exit {
                    // The process reaper and private FD worker race to deliver
                    // their final events. Keep the owned terminal until the
                    // bounded stop deadline lets the receipt worker settle.
                    if live.stop.is_some() && live.receipt.is_none() { continue; }
                    // Physical exit alone never certifies final bytes. Even an
                    // observed completed model turn still needs the FD receipt.
                    let proved = live.receipt.is_some();
                    app.fabric_runtime.close_managed_assignment(assignment); run.terminal = None; run.completed_at = None;
                    if proved { live.prepared.confirm_transient_settlement(); }
                    run.status = if proved { AgentStatus::Paused } else { AgentStatus::Orphaned };
                    run.last_error = Some(if proved { "managed helper stopped with retained checkpoint; source acceptance remains pending".into() }
                        else { "managed helper exited without a matching shutdown receipt; recovery required".into() });
                    let _ = save_native_run_record(&app.cwd, run); self.live.remove(&id); app.dirty = true;
                } else if exit.is_err() {
                    live.uncertain = true;
                }
            }
            // Oldest first, with one aggregate admission budget shared with
            // preparation. Waiting keys stay in their host-owned prepared value.
            let mut waiting: Vec<_> = self.live.iter().filter_map(|(id,live)|live.auth.as_ref()
                .filter(|auth|!auth.running && !live.uncertain && live.stop.is_none()).map(|auth|(auth.deadline,id.clone()))).collect();
            waiting.sort_unstable();
            for (_,id) in waiting {
                let Ok(permit) = self.queue.admit() else { break; };
                let live = self.live.get_mut(&id).unwrap();
                let auth = live.auth.as_mut().unwrap();
                if Instant::now() >= auth.deadline || auth.cancelled.load(Ordering::Acquire) { live.uncertain = true; continue; }
                let Some(key) = live.prepared.take_codex_api_key() else { live.uncertain = true; continue; };
                let fence = live.prepared.assignment.fence.clone(); let panic_fence = fence.clone();
                let process = auth.process.clone(); let panic_process = process.clone(); let cancelled = auth.cancelled.clone();
                let deadline = auth.deadline; let launch = live.prepared.link_launch();
                auth.running = true;
                if self.queue.spawn(permit, app.pty_output_waker.clone(), move |_| {
                    let result = agent_link::authenticate_managed_codex_api_key(&launch, process.clone(), key, &cancelled, deadline);
                    WorkerResult { fence, result:Ok(WorkerStep::Authenticated { process, result }) }
                }, move ||WorkerResult { fence:panic_fence, result:Ok(WorkerStep::Authenticated { process:panic_process,
                    result:Err(agent_link::ManagedCodexAuthError::Transport) }) }).is_err() { live.uncertain = true; }
            }
            Ok(())
        }
    }

    fn orphan(repo: &Path, run: &mut AgentRun, reason: &str) {
        run.status = AgentStatus::Orphaned; run.completed_at = None; run.last_error = Some(reason.into());
        let _ = save_native_run_record(repo, run);
    }

    pub(crate) fn stop(app: &mut App, index: usize) -> Option<bool> {
        let run = app.agents.get_mut(index)?;
        run.managed_local.as_ref()?;
        app.pending_enters.retain(|pending|pending.run_id != run.id);
        if let Some(pending) = app.managed_local.pending.get(&run.id) {
            pending.cancelled.store(true, Ordering::Release);
            run.status = AgentStatus::Stopped; run.completed_at = None;
            run.last_error = Some("managed launch cancelled before helper adoption".into());
        } else if let Some(live) = app.managed_local.live.get_mut(&run.id) {
            if let Some(auth) = &live.auth { auth.cancelled.store(true, Ordering::Release); }
            if live.stop.is_none() && !live.uncertain {
                let assignment = &live.prepared.assignment; let fence = &assignment.fence;
                let request = StopAndCheckpoint { request_id: uuid::Uuid::new_v4().to_string(), execution_id: fence.attempt_id.clone(),
                    operation_id: assignment.observation.attempt.operation_id.clone(), view_id: assignment.observation.view_id.clone(),
                    run_id: fence.run_id.clone(), run_epoch: fence.run_epoch, owner: fence.owner_epoch() };
                let result = run.terminal.as_ref().context("managed terminal is absent").and_then(|terminal|
                    app.fabric_runtime.begin_managed_shutdown(&fence.owner, assignment, terminal))
                    .and_then(|_|live.control.stop_and_checkpoint(request.clone()));
                if result.is_ok() { live.stop = Some((request, Instant::now())); }
                else { live.uncertain = true; }
            }
            run.last_error = Some("stopping exact managed helper; awaiting checkpoint and physical exit".into());
        } else {
            orphan(&app.cwd, run, "managed source requires explicit owned recovery; no legacy stop/resume path");
        }
        let _ = save_native_run_record(&app.cwd, run); app.dirty = true;
        Some(true)
    }

    pub(crate) fn abandon(app: &mut App, run_id: &str) {
        if let Some(pending) = app.managed_local.pending.get(run_id) { pending.cancelled.store(true, Ordering::Release); }
        app.fabric_runtime.close(run_id);
        // Dropping the handle closes the private control channel. The helper
        // must withhold checkpoint authority and retains its orphan resources.
        app.managed_local.live.remove(run_id);
    }

    pub(crate) fn accepts_input(app: &App, index: usize) -> bool {
        let Some(run) = app.agents.get(index) else { return false; };
        if run.managed_local.is_none() { return true; }
        app.managed_local.live.get(&run.id).is_some_and(|live|live.backend_ready && !live.uncertain && live.stop.is_none()
            && matches_workspace(run.managed_local.as_ref(), &live.prepared.assignment, false))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn cancelled_or_timed_out_workers_hold_admission_until_actual_settlement() {
            let queue = WorkerQueue::new();
            let (release, receive) = mpsc::sync_channel(1);
            let cancelled = Arc::new(AtomicBool::new(false));
            let observed = cancelled.clone();
            let first = queue.admit().unwrap();
            queue.spawn(first, None, move |_| { receive.recv().unwrap(); observed.load(Ordering::Acquire) }, ||false).unwrap();
            let second = queue.admit().unwrap();
            cancelled.store(true, Ordering::Release);
            assert!(queue.admit().is_err(), "cancellation cannot release an executing worker's permit");
            drop(second);
            let second = queue.admit().unwrap();
            assert!(queue.admit().is_err());
            release.send(()).unwrap();
            assert!(queue.receive.recv_timeout(Duration::from_secs(2)).unwrap());
            let deadline = Instant::now() + Duration::from_secs(2);
            while queue.active.load(Ordering::Acquire) == MAX_PREPARATIONS && Instant::now() < deadline { thread::yield_now(); }
            let recovered = queue.admit().unwrap();
            drop(recovered); drop(second);
            assert_eq!(queue.active.load(Ordering::Acquire), 0);
        }
        #[test]
        fn actual_worker_panic_retires_exact_pending_state_and_preserves_cancellation() {
            for cancel in [false, true] {
                let mut app = App::new(); fs::create_dir_all(&app.cwd).unwrap();
                let mut journal = Journal::open(app.cwd.join("journal.sqlite")).unwrap();
                let owner = journal.create_project("p", "owner", "base", 0).unwrap();
                let attempt_id = uuid::Uuid::new_v4().to_string();
                let run_epoch = journal.observe_run_attempt(&owner, "run", &attempt_id, 1).unwrap();
                let fence = ManagedLaunchFence { owner:owner.clone(), run_id:"run".into(), run_epoch,
                    attempt_id, request_id:uuid::Uuid::new_v4().to_string() };
                let marker = ManagedRunSource::Reserved { fence:fence.clone(), runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot, b"fixture") };
                let mut run = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({"id":"run","task":"owned","managedLocal":marker})).unwrap();
                run.managed_local = Some(marker); run.status = AgentStatus::Running; app.agents.push(run);
                let mut state = ManagedAppState::default();
                let cancelled = Arc::new(AtomicBool::new(false));
                state.pending.insert("run".into(), Pending { fence:fence.clone(), cancelled:cancelled.clone(), started:Instant::now() });
                let permit = state.queue.admit().unwrap();
                let (release, receive) = mpsc::sync_channel(1);
                state.queue.spawn(permit, None, move |_| { receive.recv().unwrap(); panic!("fixture panic payload must not enter App notices"); },
                    move ||WorkerResult { fence, result:Err(anyhow::anyhow!("managed preparation worker failed without a result")) }).unwrap();
                if cancel { cancelled.store(true, Ordering::Release); app.agents[0].status = AgentStatus::Stopped; }
                assert_eq!(state.queue.active.load(Ordering::Acquire), 1);
                release.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while (!state.pending.is_empty() || state.queue.active.load(Ordering::Acquire) != 0) && Instant::now() < deadline {
                    state.poll(&mut app, &mut journal, &owner).unwrap(); thread::yield_now();
                }
                assert!(state.pending.is_empty(), "settled panics must not leave phantom reservations");
                assert_eq!(state.queue.active.load(Ordering::Acquire), 0);
                assert_eq!(app.agents[0].status, if cancel { AgentStatus::Stopped } else { AgentStatus::Orphaned });
                assert!(!app.agents[0].last_error.as_deref().unwrap_or("").contains("fixture panic payload"));
                assert!(state.queue.admit().is_ok(), "actual settlement restores bounded admission");
                let _ = fs::remove_dir_all(&app.cwd);
            }
        }
        #[test]
        fn late_results_cannot_revive_cancelled_removed_or_replaced_reservations() {
            for kind in ["stopped", "removed", "replacement", "new-request"] {
                let mut app = App::new(); fs::create_dir_all(&app.cwd).unwrap();
                let mut journal = Journal::open(app.cwd.join("journal.sqlite")).unwrap();
                let owner = journal.create_project("p", "owner", "base", 0).unwrap();
                let attempt = uuid::Uuid::new_v4().to_string();
                let epoch = journal.observe_run_attempt(&owner, "run", &attempt, 1).unwrap();
                let fence = ManagedLaunchFence { owner:owner.clone(), run_id:"run".into(), run_epoch:epoch,
                    attempt_id:attempt, request_id:uuid::Uuid::new_v4().to_string() };
                let marker = ManagedRunSource::Reserved { fence:fence.clone(), runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot, b"fixture") };
                let mut run = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({"id":"run","task":"owned","managedLocal":marker})).unwrap();
                run.managed_local = Some(marker); run.status = AgentStatus::Running; app.agents.push(run);
                assert!(still_pending(&app, &fence, &owner));
                let cancelled = Arc::new(AtomicBool::new(false));
                let mut state = ManagedAppState::default();
                state.pending.insert("run".into(), Pending { fence:fence.clone(), cancelled:cancelled.clone(), started:Instant::now() });
                match kind {
                    "stopped" => app.agents[0].status = AgentStatus::Stopped,
                    "removed" => app.agents.clear(),
                    _ => {
                        let mut next = fence.clone(); next.request_id = uuid::Uuid::new_v4().to_string();
                        if kind == "replacement" { next.attempt_id = uuid::Uuid::new_v4().to_string();
                            next.run_epoch = journal.observe_run_attempt(&owner, "run", &next.attempt_id, 2).unwrap(); }
                        app.agents[0].managed_local = Some(ManagedRunSource::Reserved { fence:next, runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot, b"fixture") });
                    }
                }
                assert!(!still_pending(&app, &fence, &owner));
                assert!(state.queue.send.send(WorkerResult { fence, result:Err(anyhow::anyhow!("late fixture result")) }).is_ok());
                state.poll(&mut app, &mut journal, &owner).unwrap();
                assert!(cancelled.load(Ordering::Acquire));
                assert!(state.pending.is_empty());
                assert!(state.live.is_empty());
                if kind == "removed" { assert!(app.agents.is_empty()); }
                else { assert_eq!(app.agents[0].status, if kind == "stopped" { AgentStatus::Stopped } else { AgentStatus::Running }); }
                let _ = fs::remove_dir_all(&app.cwd);
            }
        }
        #[test]
        fn report_reads_are_bounded_dirfd_scoped_and_never_promote_guest_claims() {
            use std::os::unix::fs::symlink;
            let root = env::temp_dir().join(format!("rudder-managed-report-{}", uuid::Uuid::new_v4()));
            let home = root.join("home"); let reports = home.join(".rudder-reports");
            fs::create_dir_all(&reports).unwrap();
            let paths = managed_launch::ManagedReportMapping { host_done_file:reports.join("done.json"), host_signal_file:reports.join("signal.json"),
                guest_done_file:"/home/agent/.rudder-reports/done.json".into(), guest_signal_file:"/home/agent/.rudder-reports/signal.json".into() };
            fs::write(&paths.host_done_file, br#"{"node":"peer","run_id":"peer","summary":"candidate only","delivery":{"status":"verified"},"checkpoint":"forged"}"#).unwrap();
            fs::write(&paths.host_signal_file, br#"{"state":"done","backend_ready":true,"ack":true}"#).unwrap();
            let mut run = crate::gitio::agent_from_run_record(&root, serde_json::json!({"id":"owned","task":"owned","status":"running"})).unwrap();
            let delivery = run.delivery.clone();
            apply_reports(&mut run, read_reports(&paths).unwrap(), true);
            assert_eq!(run.id, "owned"); assert_eq!(run.status, AgentStatus::Running);
            assert_eq!(run.done_summary.as_deref(), Some("candidate only")); assert_eq!(run.delivery, delivery);
            assert!(run.completed_at.is_none());
            fs::write(&paths.host_signal_file, br#"{"state":"permission"}"#).unwrap();
            apply_reports(&mut run, read_reports(&paths).unwrap(), true); assert!(run.needs_permission);
            run.status = AgentStatus::Paused; run.needs_permission = false; run.wait_signal = None;
            apply_reports(&mut run, read_reports(&paths).unwrap(), false);
            assert!(!run.needs_permission && run.wait_signal.is_none(), "late writable attention hints cannot revive a stopped pane");
            assert_eq!(run.status, AgentStatus::Paused);
            fs::write(&paths.host_done_file, vec![b'x';64*1024+1]).unwrap(); assert!(read_reports(&paths).is_err());
            fs::remove_file(&paths.host_done_file).unwrap();
            let outside = root.join("outside"); fs::create_dir(&outside).unwrap(); fs::write(outside.join("done.json"), b"private fixture").unwrap();
            symlink(outside.join("done.json"), &paths.host_done_file).unwrap(); assert!(read_reports(&paths).is_err());
            fs::remove_file(&paths.host_done_file).unwrap(); fs::remove_file(&paths.host_signal_file).unwrap(); fs::remove_dir(&reports).unwrap();
            symlink(&outside, &reports).unwrap(); assert!(read_reports(&paths).is_err(), "a writable parent symlink cannot expose host files");
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use local::{ManagedAppState, poll, stop, abandon, accepts_input};

#[cfg(not(target_os = "linux"))]
#[derive(Default)]
pub(crate) struct ManagedAppState;
#[cfg(not(target_os = "linux"))]
pub(crate) fn poll(_: &mut App, _: &mut Journal, _: &OwnerFence) -> Result<()> { Ok(()) }
#[cfg(not(target_os = "linux"))]
pub(crate) fn stop(app: &mut App, index: usize) -> Option<bool> {
    let run = app.agents.get_mut(index)?;
    run.managed_local.as_ref()?;
    run.status = AgentStatus::Orphaned;
    run.last_error = Some("managed local execution requires a qualified Linux worker".into());
    Some(false)
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn abandon(_: &mut App, _: &str) {}
#[cfg(not(target_os = "linux"))]
pub(crate) fn accepts_input(app: &App, index: usize) -> bool { app.agents.get(index).is_some_and(|run|run.managed_local.is_none()) }
#[cfg(not(target_os = "linux"))]
impl App {
    pub(crate) fn start_managed_local(&mut self, _: usize, _: ManagedLocalLaunchInputs, _: PathBuf,
        _: &mut Journal, _: &OwnerFence) -> Result<ManagedLaunchFence> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "managed local execution requires a qualified Linux worker").into())
    }
}

#[cfg(test)]
#[path = "managed_app_tests.rs"]
mod integration_tests;
