use super::*;

fn slot() -> RuntimeLink {
    RuntimeLink {
        attempt: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
        epoch: 1,
        profile: "claude-sdk".into(),
        backend: Backend::Claude,
        transport_uncertain: false,
        link: None,
        broker: None,
        broker_work: None,
        projection_hash: None,
        broker_failed_hash: None,
        thread: Some("thread-1".into()),
        active: Some("turn-1".into()),
        interrupted: None,
        phase: RuntimePhase::Working,
        qualified: true,
        uncertain: false,
        closed: false,
    }
}

#[test]
fn runtime_capabilities_require_owned_qualified_exact_turn() {
    let mut link = slot();
    assert!(link
        .observation("target", Backend::Claude, true)
        .capabilities
        .contains("interrupt"));
    assert!(!link
        .observation("target", Backend::Claude, false)
        .capabilities
        .contains("interrupt"));
    link.uncertain = true;
    assert_eq!(
        link.observation("target", Backend::Claude, true)
            .capabilities,
        BTreeSet::from(["inspect".into(), "cancel".into()])
    );
    link.uncertain = false;
    link.active = None;
    link.interrupted = Some("turn-1".into());
    link.phase = RuntimePhase::Interrupted;
    let capabilities = link
        .observation("target", Backend::Claude, true)
        .capabilities;
    assert!(capabilities.contains("resume"));
    assert!(!capabilities.contains("deliver"));
}

#[test]
fn requested_sdk_binding_allows_only_first_idle_input_before_qualification() {
    let mut link = slot();
    link.qualified = false;
    link.active = None;
    link.phase = RuntimePhase::Waiting;
    assert!(link
        .observation("target", Backend::Claude, true)
        .capabilities
        .contains("deliver"));
    assert!(!link
        .observation("target", Backend::Claude, true)
        .capabilities
        .contains("steer"));
    link.thread = None;
    assert!(!link
        .observation("target", Backend::Claude, true)
        .capabilities
        .contains("deliver"));
}

fn test_agent_run(id: &str, task: &str) -> AgentRun {
    AgentRun {
        id: id.to_string(),
        created_at: "1".to_string(),
        mode: AgentMode::Execute,
        task: task.to_string(),
        plan_id: None,
        reviewed_at: None,
        task_summary: summarize_task(task),
        current_prompt: task.to_string(),
        turns: vec![AgentTurn {
            ts: "1".to_string(),
            prompt: task.to_string(),
            source: "user".to_string(),
        }],
        last_user_input_at: "1".to_string(),
        backend: Backend::Claude,
        model: "sonnet".to_string(),
        effort: None,
        status: AgentStatus::Running,
        cwd: std::env::temp_dir(),
        workspace_branch: None,
        workspace_path: None,
        workspace_name: None,
        jj_change_id: None,
        integration: IntegrationEvidence::default(),
        publish: PublishEvidence::default(),
        delivery: DeliveryEvidence::default(),
        session_id: None,
        execution_profile: None,
        terminal: None,
        terminal_size: None,
        last_output_at: Instant::now(),
        completed_at: None,
        autosteered: false,
        plain_process: false,
        interactive_orchestrator: false,
        needs_permission: false,
        needs_user_input: false,
        wait_signal: None,
        worker_exit_note: None,
        last_error: None,
        worker_input_draft: String::new(),
        worker_input_cursor: 0,
        worker_input_is_prompt: false,
        last_drain_at: None,
        review_source_ids: Vec::new(),
        deps: Vec::new(),
        soft_deps: Vec::new(),
        node_id: None,
        reconcile_planner: false,
        swarm_experiment: false,
        plan_stream: None,
        plan_output_cache: None,
        last_worker_input_at: None,
        merge_resolver: false,
        merge_conflict: false,
        merge_conflict_operation: ConflictOperation::Merge,
        merge_conflict_files: Vec::new(),
        had_merge_conflict: false,
        done_summary: None,
        tokens_in: 0,
        tokens_out: 0,
        gam: None,
    }
}

struct Fixture {
    dir: PathBuf,
    app: App,
    journal: Journal,
    owner: OwnerFence,
    runtime: RuntimeState,
}
impl Fixture {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("rudder-runtime-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir(&dir).unwrap();
        let mut journal = Journal::open(dir.join("journal.sqlite")).unwrap();
        let owner = journal
            .create_project("project", "owner", "base", 1000)
            .unwrap();
        journal
            .observe_run_attempt(
                &owner,
                "target",
                "51001c4a-c30d-4651-a2ef-a558a9302e5f",
                1000,
            )
            .unwrap();
        let mut app = App::new();
        app.cwd = dir.clone();
        let mut run = test_agent_run("target", "owned fixture only");
        run.cwd = dir.clone();
        run.execution_profile = Some("claude-sdk".into());
        let mut command = TerminalCommand::with_args("/bin/sh", ["-c", "exec sleep 120"]);
        command.env.push((
            "RUDDER_FABRIC_ATTEMPT_ID".into(),
            "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
        ));
        run.terminal = Some(
            TerminalPane::spawn_shell_or_command(Some(command), TerminalPaneOptions::default())
                .unwrap(),
        );
        app.agents = vec![run];
        let mut runtime = RuntimeState::default();
        runtime.links.insert("target".into(), slot());
        Self {
            dir,
            app,
            journal,
            owner,
            runtime,
        }
    }
    fn event(&mut self, kind: AgentLinkEventKind) {
        self.runtime
            .event(
                &mut self.app,
                &mut self.journal,
                &self.owner,
                AgentLinkEvent {
                    run_id: "target".into(),
                    attempt_id: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
                    kind,
                },
                1001,
            )
            .unwrap();
    }
    fn command(&mut self) -> EffectClaim {
        use rudder_engine::journal::CommandRequest;
        self.journal
            .register_run(&self.owner, "sender", 1000)
            .unwrap();
        let token = self
            .journal
            .issue_capability(
                &self.owner,
                &CapabilityGrant {
                    principal: "real-principal".into(),
                    run_id: "sender".into(),
                    run_epoch: 1,
                    operations: BTreeSet::from(["command:interrupt".into()]),
                    recipients: BTreeMap::from([("target".into(), 1)]),
                    scopes: BTreeSet::from(["project".into()]),
                    topics: BTreeSet::new(),
                    expires_at_ms: None,
                },
                1000,
            )
            .unwrap();
        self.runtime
            .publish(&self.app, &mut self.journal, &self.owner, 1000)
            .unwrap();
        let receipt=self.journal.accept_command(&token,&CommandRequest{idempotency_key:"interrupt-1".into(),target_run:"target".into(),target_epoch:1,kind:"interrupt".into(),scope:"project".into(),payload:serde_json::json!({"expected_thread_id":"thread-1","expected_turn_id":"turn-1"})},1000).unwrap();
        self.journal
            .claim_effect(&self.owner, &receipt.command_id, 1001)
            .unwrap()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for run in &mut self.app.agents {
            if let Some(mut terminal) = run.terminal.take() {
                terminal.terminate_and_wait();
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn typed_idle_interrupt_and_completion_have_distinct_app_lifecycles() {
    let mut f = Fixture::new();
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
        status: "idle".into(),
    });
    assert_eq!(f.app.agents[0].status, AgentStatus::Running);
    assert!(f.app.agents[0].needs_user_input);
    assert!(f.app.agents[0].completed_at.is_none());
    f.event(AgentLinkEventKind::TurnStarted {
        thread_id: "thread-1".into(),
        turn_id: "turn-2".into(),
    });
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "thread-1".into(),
        turn_id: "turn-2".into(),
        status: "interrupted".into(),
    });
    assert_eq!(f.app.agents[0].status, AgentStatus::Paused);
    assert!(f.app.agents[0].completed_at.is_none());
    f.event(AgentLinkEventKind::TurnStarted {
        thread_id: "thread-1".into(),
        turn_id: "turn-3".into(),
    });
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "thread-1".into(),
        turn_id: "turn-3".into(),
        status: "completed".into(),
    });
    assert_eq!(f.app.agents[0].status, AgentStatus::Done);
    assert!(f.app.agents[0].completed_at.is_some());
    assert!(f.dir.join(".rudder/runs/target/run.json").exists());
}

#[test]
fn foreign_thread_turn_and_attempt_cannot_complete_or_revive_stopped_work() {
    let mut f = Fixture::new();
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "other-thread".into(),
        turn_id: "turn-1".into(),
        status: "completed".into(),
    });
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "thread-1".into(),
        turn_id: "other-turn".into(),
        status: "completed".into(),
    });
    assert_eq!(f.app.agents[0].status, AgentStatus::Running);
    f.app.agents[0].status = AgentStatus::Stopped;
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
        status: "completed".into(),
    });
    f.event(AgentLinkEventKind::TurnStarted {
        thread_id: "thread-1".into(),
        turn_id: "late-turn".into(),
    });
    assert_eq!(f.app.agents[0].status, AgentStatus::Stopped);
    f.app.agents[0].status = AgentStatus::Paused;
    f.app.agents[0].terminal = None;
    f.event(AgentLinkEventKind::TurnStarted {
        thread_id: "thread-1".into(),
        turn_id: "after-close".into(),
    });
    assert_eq!(
        f.app.agents[0].status,
        AgentStatus::Paused,
        "no owned PTY must never persist running"
    );
}

#[test]
fn uncertain_effect_stays_blocked_then_exact_late_receipt_reconciles_without_replay() {
    let mut f = Fixture::new();
    let claim = f.command();
    let id = claim.command_id.clone();
    f.runtime.pending.insert(
        id.clone(),
        PendingControl {
            claim,
            attempt: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
            deadline: Instant::now(),
            uncertain: false,
            reserved_status: None,
        },
    );
    f.runtime
        .uncertain(&mut f.journal, &id, "fixture transport timed out", 1002)
        .unwrap();
    assert!(f.runtime.pending[&id].uncertain);
    assert!(f.runtime.links["target"].uncertain);
    assert_eq!(
        f.journal
            .commands_in_state(
                &f.owner,
                rudder_engine::journal::CommandState::Uncertain,
                10
            )
            .unwrap()
            .len(),
        1
    );
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
        status: "interrupted".into(),
    });
    f.event(AgentLinkEventKind::CommandResult {
        command_id: id.clone(),
        outcome: AgentLinkOutcome::InterruptConfirmed {
            turn_id: "turn-1".into(),
        },
    });
    assert!(!f.runtime.pending.contains_key(&id));
    assert!(!f.runtime.links["target"].uncertain);
    assert_eq!(f.app.agents[0].status, AgentStatus::Paused);
    let complete = f
        .journal
        .commands_in_state(
            &f.owner,
            rudder_engine::journal::CommandState::Completed,
            10,
        )
        .unwrap();
    assert_eq!(complete.len(), 1);
    assert_eq!(
        complete[0].result.as_ref().unwrap()["adapter"]["status"],
        "interrupt_confirmed"
    );
    assert!(f.journal.ready_commands(&f.owner, 10).unwrap().is_empty());
}

#[test]
fn old_receipt_can_settle_history_but_never_mutate_replacement_attempt() {
    let mut f = Fixture::new();
    let claim = f.command();
    let id = claim.command_id.clone();
    f.runtime.pending.insert(
        id.clone(),
        PendingControl {
            claim,
            attempt: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
            deadline: Instant::now(),
            uncertain: false,
            reserved_status: None,
        },
    );
    f.journal
        .observe_run_attempt(
            &f.owner,
            "target",
            "51001c4a-c30d-4651-a2ef-a558a9302e50",
            1002,
        )
        .unwrap();
    let mut replacement = slot();
    replacement.attempt = "51001c4a-c30d-4651-a2ef-a558a9302e50".into();
    replacement.epoch = 2;
    f.runtime.links.insert("target".into(), replacement);
    f.event(AgentLinkEventKind::CommandResult {
        command_id: id.clone(),
        outcome: AgentLinkOutcome::InterruptConfirmed {
            turn_id: "turn-1".into(),
        },
    });
    assert_eq!(f.app.agents[0].status, AgentStatus::Running);
    assert_eq!(
        f.runtime.links["target"].attempt,
        "51001c4a-c30d-4651-a2ef-a558a9302e50"
    );
    assert_eq!(
        f.journal
            .commands_in_state(
                &f.owner,
                rudder_engine::journal::CommandState::Completed,
                10
            )
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn broker_start_failure_is_scoped_and_a_retired_result_cannot_replace_current_tools() {
    let mut f = Fixture::new();
    let hash = [7; 32];
    let cancelled = Arc::new(AtomicBool::new(false));
    f.runtime.links.get_mut("target").unwrap().broker_work = Some(BrokerWork {
        hash,
        cancelled: Arc::clone(&cancelled),
    });
    f.runtime
        .broker_tx
        .send(BrokerResult {
            run_id: "target".into(),
            attempt: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
            run_epoch: 1,
            owner_epoch: f.owner.epoch,
            hash,
            result: Err(anyhow::anyhow!("owned fixture lock busy")),
        })
        .unwrap();
    f.runtime.broker_results(&mut f.app, &f.owner);
    assert_eq!(f.runtime.links["target"].broker_failed_hash, Some(hash));
    assert!(f.runtime.links["target"].broker.is_none());
    assert!(f
        .app
        .notice
        .as_deref()
        .unwrap_or("")
        .contains("agent tools unavailable"));
    f.event(AgentLinkEventKind::TurnCompleted {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
        status: "idle".into(),
    });
    assert!(
        f.app.agents[0].needs_user_input,
        "one broker failure must not stop App lifecycle processing"
    );
    f.app.notice = None;
    f.runtime
        .broker_tx
        .send(BrokerResult {
            run_id: "target".into(),
            attempt: "retired-attempt".into(),
            run_epoch: 1,
            owner_epoch: f.owner.epoch,
            hash,
            result: Err(anyhow::anyhow!("obsolete result")),
        })
        .unwrap();
    f.runtime.broker_results(&mut f.app, &f.owner);
    assert!(
        f.app.notice.is_none(),
        "a stale startup result has no authority in another attempt"
    );
}

#[test]
fn termination_callback_cancels_broker_startup_before_pty_shutdown() {
    let mut link = slot();
    let cancelled = Arc::new(AtomicBool::new(false));
    link.broker_work = Some(BrokerWork {
        hash: [8; 32],
        cancelled: Arc::clone(&cancelled),
    });
    let callback = link.close_callback();
    assert!(!cancelled.load(Ordering::Acquire));
    callback();
    assert!(cancelled.load(Ordering::Acquire));
}

#[test]
fn bound_codex_thread_does_not_qualify_controls_without_server_version_evidence() {
    let mut f = Fixture::new();
    let slot = f.runtime.links.get_mut("target").unwrap();
    slot.profile = "codex-app-server".into();
    slot.backend = Backend::Codex;
    slot.qualified = false;
    f.event(AgentLinkEventKind::Bound {
        thread_id: "thread-1".into(),
        active_turn_id: Some("turn-1".into()),
        last_interrupted_turn_id: None,
        steer_mode: crate::agent_link::AgentSteerMode::ActiveAppend,
    });
    assert!(!f
        .journal
        .runtime_snapshot(&f.owner, "target", 1001)
        .unwrap()
        .unwrap()
        .observation
        .capabilities
        .contains("interrupt"));
    f.event(AgentLinkEventKind::ProfileObserved {
        qualified: true,
        uncertain: false,
    });
    assert!(f
        .journal
        .runtime_snapshot(&f.owner, "target", 1001)
        .unwrap()
        .unwrap()
        .observation
        .capabilities
        .contains("interrupt"));
}
