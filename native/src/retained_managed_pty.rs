//! Linux task-only activation custody. No scheduling, Journal or model work.
use super::*;
use rudder_engine::execution::{finalization_budget::FinalizationBudget,
    handoff::{HandoffReceipt, ManagedExecutionHandle, ManagedExecutionHandoff,
        ManagedExecutionStartBarrier, OwnedWorkerJoin, RetainedManagedExecutionActivation, WorkerJoined}};

#[cfg(test)]
#[path = "retained_managed_pty_tests.rs"]
mod tests;

/// Staged BEFORE PTY/process effects. Every fallible operation borrows this
/// owner; error or caught unwind transfers this SAME object to the supervisor.
/// Drop is not settlement. Keep it until explicit off-App cleanup succeeds.
#[must_use = "retain original PTY/child/control custody through off-App settlement"]
pub struct ManagedPtyActivationOwner {
    activation: RetainedManagedExecutionActivation,
    pty: RetainedPtySpawn,
    attempted: bool,
    transferred: bool,
    settled: bool,
}
impl ManagedPtyActivationOwner {
    pub fn new(handoff: ManagedExecutionHandoff) -> Self {
        Self { activation: RetainedManagedExecutionActivation::new(handoff),
            pty: RetainedPtySpawn::default(), attempted: false, transferred: false, settled: false }
    }
    /// App only creates/mutates the terminal. This starts bounded workers and
    /// returns typed custody; readiness and actual backend drain remain separate.
    pub fn activate(&mut self, native_program: impl Into<String>, attempt_id: &str,
        execution_profile: &str, launch_json: Option<&str>, options: TerminalPaneOptions,
        budget: &FinalizationBudget, cancelled: Arc<AtomicBool>)
        -> Result<(TerminalPane, HandoffReceipt, ManagedExecutionHandle, ManagedExecutionStartBarrier)> {
        anyhow::ensure!(!self.attempted, "retained managed PTY activation is one-shot");
        self.attempted = true;
        activation_time(budget, &cancelled)?;
        anyhow::ensure!(uuid::Uuid::parse_str(attempt_id).is_ok_and(|id| id.to_string() == attempt_id), "managed PTY requires its reserved canonical attempt UUID");
        anyhow::ensure!(matches!(execution_profile, "claude-sdk" | "codex-app-server"), "managed PTY requires a qualified backend profile");
        let binding = self.activation.binding();
        anyhow::ensure!(binding.execution_id == attempt_id && binding.execution_profile == execution_profile, "managed PTY identity differs from its sealed handoff");
        let mut command = TerminalCommand::with_args(native_program, self.activation.helper_args())
            .with_env("RUDDER_FABRIC_ATTEMPT_ID", attempt_id)
            .with_env("RUDDER_EXECUTION_PROFILE", execution_profile);
        if let Some(launch) = launch_json { command = command.with_env("RUDDER_AGENT_LINK_LAUNCH", launch); }
        self.pty.spawn(Some(command), options)?;
        activation_time(budget, &cancelled)?;
        let pid = self.pty.pane.as_ref().unwrap().child_process_id().context("managed PTY has no owned helper PID")?;
        self.activation.stage_bounded(pid, budget, cancelled.clone())?;
        activation_time(budget, &cancelled)?;
        retaining_test_step(RetainingStep::AfterActivation)?;
        let barrier = self.activation.take_start_barrier()?;
        let (receipt, control) = self.activation.take_live()?;
        let pane = self.pty.pane.take().unwrap();
        self.transferred = true;
        Ok((pane, receipt, control, barrier))
    }
    /// OFF App, borrowed and retryable. Kill request, actual direct-child wait,
    /// actual reader TLS join, and NO possible descriptor dispatch are ALL
    /// required. A started/uncertain control worker never implies no backend.
    pub fn settle_unused_off_app(&mut self, budget: &FinalizationBudget) -> Result<()> {
        budget.remaining()?;
        if self.settled { return Ok(()); }
        anyhow::ensure!(!self.transferred, "live PTY custody already transferred");
        self.activation.close();
        self.pty.settle_off_app(budget)?;
        self.activation.settle_undispatched_off_app(budget)?;
        budget.remaining()?; self.settled = true; Ok(())
    }
    pub fn is_settled(&self) -> bool { self.settled }
}

/// Cleanup custody only. App moves the ORIGINAL pane here before replacing or
/// dropping a task's terminal. No saved PID/status/JSON can construct a pane.
/// This proves neither successful helper execution nor backend/writer drain.
#[must_use = "retain original PTY reader/child until actual off-App settlement"]
pub struct ManagedPtyDrainOwner { pty: RetainedPtySpawn }
impl ManagedPtyDrainOwner {
    /// App-only pure move AFTER App observed the original child's actual reap.
    /// Live/refused transfers return the SAME pane, including its reader handle.
    pub fn retire(pane: TerminalPane, custody: &ManagedHelperCustody) -> std::result::Result<Self,(TerminalPane,anyhow::Error)> {
        if !pane.matches_managed_helper_custody(custody) || pane.child.status.is_none() {
            return Err((pane,anyhow!("original managed PTY has not been reaped by App")));
        }
        Ok(Self { pty: RetainedPtySpawn { pane: Some(pane), allow_runtime_callback: true, ..Default::default() } })
    }
    pub fn settle_off_app(&mut self, budget: &FinalizationBudget) -> Result<()> {
        anyhow::ensure!(self.pty.pane.as_ref().is_some_and(|pane|pane.child.status.is_some()), "retired pane lost original App reap proof");
        self.pty.settle_off_app(budget)
    }
    pub fn is_settled(&self) -> bool { self.pty.settled }
}

fn activation_time(budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed PTY activation cancelled");
    budget.remaining()?; Ok(())
}

#[derive(Default)]
struct RetainedPtySpawn {
    pair: Option<portable_pty::PtyPair>,
    master: Option<Box<dyn MasterPty + Send>>,
    reservation: Option<ChildReservation>,
    child: Option<OwnedPtyChild>,
    // A spawn error drops only a clone of this already retained work cell.
    reader_work: Option<Arc<Mutex<Option<PtyReaderWork>>>>,
    reader_thread: Option<JoinHandle<()>>,
    reader_join: Option<OwnedWorkerJoin>,
    reader_joined: Option<WorkerJoined>,
    reader_stop: Option<Arc<AtomicBool>>,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    reader: Option<Box<dyn Read + Send>>,
    pane: Option<TerminalPane>,
    allow_runtime_callback: bool,
    spawn_in_flight: bool,
    attempted: bool,
    settled: bool,
}
impl RetainedPtySpawn {
    fn spawn(&mut self, command: Option<TerminalCommand>, options: TerminalPaneOptions) -> Result<()> {
        anyhow::ensure!(!self.attempted, "retained PTY spawn is one-shot"); self.attempted = true;
        self.reservation = Some(child_reaper()?.reserve()?);
        let instance_id = command.as_ref().and_then(|cmd| cmd.env.iter().rev().find(|(key, _)| key == "RUDDER_FABRIC_ATTEMPT_ID"))
            .and_then(|(_, value)| uuid::Uuid::parse_str(value).ok()).unwrap_or_else(uuid::Uuid::new_v4).to_string();
        let launch_env = |name: &str| command.as_ref().and_then(|cmd| cmd.env.iter().rev().find(|(key, _)| key == name)).map(|(_, value)| value.clone());
        let execution_profile = launch_env("RUDDER_EXECUTION_PROFILE");
        let agent_link_launch_json = launch_env("RUDDER_AGENT_LINK_LAUNCH");
        self.pair = Some(portable_pty::native_pty_system().openpty(options.size.pty_size()).context("failed to open PTY")?);
        let reader_readiness = PtyReadiness::new(self.pair.as_ref().unwrap().master.as_raw_fd()).context("failed to initialize PTY reader readiness")?;
        let mut builder = command_builder(command, &options.term)?;
        if let Some(cwd) = &options.cwd { builder.cwd(cwd); }
        // ONLY the pinned native Linux portable-pty 0.9 Unix backend is used.
        // Its ordinary Err is pre-spawn or std exec-error + actual wait. An
        // unwind before an owned child is returned stays uncertain instead.
        self.spawn_in_flight = true;
        let spawned = self.pair.as_ref().unwrap().slave.spawn_command(builder);
        let child = match spawned {
            Ok(child) => child,
            Err(error) => { self.spawn_in_flight = false; return Err(error.context("failed to spawn command in PTY")); }
        };
        self.child = Some(OwnedPtyChild::new(child, self.reservation.take().unwrap()));
        self.spawn_in_flight = false;
        let pair=self.pair.take().unwrap();self.master=Some(pair.master);drop(pair.slave);
        retaining_test_step(RetainingStep::AfterChild)?;
        self.reader = Some(self.master.as_ref().unwrap().try_clone_reader().context("failed to clone PTY reader")?);
        retaining_test_step(RetainingStep::AfterReader)?;
        self.writer = Some(Arc::new(Mutex::new(self.master.as_ref().unwrap().take_writer().context("failed to open PTY writer")?)));
        retaining_test_step(RetainingStep::AfterWriter)?;
        let writer = self.writer.as_ref().unwrap().clone();
        let reply_writer = writer.clone();
        let wants_theme_reports = Arc::new(AtomicBool::new(false));
        let reader_wants_theme_reports = wants_theme_reports.clone();
        let (output_tx, output_rx) = mpsc::sync_channel(MAX_PTY_QUEUED_CHUNKS);
        let queued_output_bytes = Arc::new(AtomicUsize::new(0));
        let reader_queued_output = queued_output_bytes.clone();
        let output_waker: Arc<Mutex<Option<PtyOutputWaker>>> = Arc::new(Mutex::new(None));
        let reader_waker = output_waker.clone();
        let reader_stop = Arc::new(AtomicBool::new(false)); self.reader_stop = Some(reader_stop.clone());
        let stopped = reader_stop.clone();
        let work = Arc::new(Mutex::new(Some(PtyReaderWork { reader: self.reader.take().unwrap(), reader_readiness,
            reply_writer, reader_wants_theme_reports, output_tx, reader_queued_output, reader_waker, stopped })));
        self.reader_work = Some(work.clone());
        retaining_test_step(RetainingStep::BeforeReaderSpawn)?;
        #[cfg(test)] let reader_gate = tests::take_reader_gate();
        self.reader_thread = Some(thread::Builder::new().name("rudder-pty-reader".into()).spawn(move || {
            #[cfg(test)] tests::install_reader_gate(reader_gate);
            let input = work.lock().unwrap_or_else(|p| p.into_inner()).take().expect("single retained PTY reader");
            input.run();
        }).context("failed to start PTY reader thread")?);
        retaining_test_step(RetainingStep::AfterReaderSpawn)?;
        // Allocate parser state before transferring any original resource.
        let parser = vt100::Parser::new(options.size.rows, options.size.cols, options.scrollback_lines);
        let pane = TerminalPane {
            instance_id,
            execution_profile,
            agent_link_launch_json,
            before_terminate: None,
            master: self.master.take().unwrap(),
            child: self.child.take().unwrap(),
            writer,
            wants_theme_reports,
            theme_generation: colors_generation(),
            output_rx,
            queued_output_bytes,
            output_eof: false,
            exit_observed: None,
            output_only: false,
            terminated: false,
            reader_thread: self.reader_thread.take(),
            reader_stop,
            parser,
            size: options.size,
            alternate_history: Vec::new(),
            alternate_history_offset: 0,
            last_alternate_snapshot: Vec::new(),
            alternate_history_limit: options.scrollback_lines.max(DEFAULT_ROWS as usize),
            region_scrollback: Vec::new(),
            region_scrollback_offset: 0,
            region_scrollback_limit: options.scrollback_lines,
            render_generation: 0,
            tracked_scroll_region: None,
            ansi_state: AnsiTrackState::Ground,
            notification_scanner: OscNotificationScanner::default(),
            open_hyperlink: None,
            hyperlinks: std::collections::VecDeque::new(),
            styled_lines_cache: None,
            output_log: String::new(),
            output_log_limit: 200_000,
            live_screen_only: options.live_screen_only,
            output_waker,
        };
        self.pane = Some(pane);
        Ok(())
    }
    fn child_mut(&mut self) -> Option<&mut OwnedPtyChild> {
        if let Some(pane) = &mut self.pane { Some(&mut pane.child) } else { self.child.as_mut() }
    }
    fn settle_off_app(&mut self, budget: &FinalizationBudget) -> Result<()> {
        budget.remaining()?;
        if self.settled { return Ok(()); }
        if let Some(stop) = &self.reader_stop { stop.store(true, Ordering::Release); }
        if let Some(pane) = &mut self.pane {
            pane.reader_stop.store(true, Ordering::Release);
            // This unadopted pane never had a runtime close callback. The owner
            // itself will keep and wait the original child, without global handoff.
            if self.allow_runtime_callback {
                // The callback owns actor/broker close custody, never an App
                // reference; invoke it off App before disposing its pane.
                if let Some(close) = pane.before_terminate.take() { close(); }
            } else { anyhow::ensure!(pane.before_terminate.is_none(), "retained failure pane acquired a runtime callback"); }
        }
        if let Some(child) = self.child_mut() { terminate_retained_child(child)?; }
        if self.reader_join.is_none() {
            let original = if let Some(pane) = &mut self.pane { pane.reader_thread.take() } else { self.reader_thread.take() };
            if let Some(original) = original { self.reader_join = Some(OwnedWorkerJoin::new(original)); }
        }
        if self.reader_joined.is_none() {
            if let Some(reader) = &self.reader_join { self.reader_joined = Some(reader.await_joined(budget.remaining()?)?); }
        }
        loop {
            budget.remaining()?;
            let reaped = match self.child_mut() { Some(child) => child.try_wait()?.is_some(), None => true };
            if reaped { break; }
            thread::sleep(budget.phase_limit(REAP_POLL_INTERVAL)?);
        }
        anyhow::ensure!(!self.spawn_in_flight, "PTY spawn unwound without a returned outcome; actual helper absence is unknown");
        // No live reader/child remains. Close retained partial PTY handles OFF
        // App. A real wait/join proof stays held in this owner until disposal.
        self.reader = None; self.reader_work = None; self.writer = None; self.master = None; self.pair = None; self.reservation = None;
        budget.remaining()?; self.settled = true; Ok(())
    }
}

fn terminate_retained_child(child: &mut OwnedPtyChild) -> Result<()> {
    if child.try_wait()?.is_some() { return Ok(()); }
    // Only the original unreaped child permits numeric process-group signaling.
    // Never use portable-pty's sleeping SIGHUP kill or release this handle to a
    // detached reaper on the task failure path.
    // App may already have sent the original child to its reserved reaper.
    // Keep its exact observation until that owner publishes actual wait proof;
    // never signal its numeric PID again or substitute a closed channel for exit.
    if child.pending.is_some() { return Ok(()); }
    anyhow::ensure!(child.child.is_some(), "retained direct child owner absent");
    let pid = child.pid.filter(|pid| *pid > 1).context("retained PTY child has no live process identity")?;
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM); libc::kill(-(pid as libc::pid_t), libc::SIGKILL); }
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) { return Err(error.into()); }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainingStep { AfterChild, AfterReader, AfterWriter, BeforeReaderSpawn, AfterReaderSpawn, AfterActivation }
fn retaining_test_step(_step: RetainingStep) -> Result<()> {
    #[cfg(test)]
    tests::fault_step(_step)?;
    Ok(())
}
