use super::*;

#[test]
fn owned_backend_peer_is_verified_before_any_protocol_bytes_are_sent() {
    use std::io::Read;
    struct RefusedPeer(AtomicBool);
    impl OwnedPeerProcess for RefusedPeer {
        fn verify_peer(&self, _: &UnixStream) -> Result<()> {
            self.0.store(true, Ordering::Release);
            anyhow::bail!("fixture belongs to another actual backend process")
        }
        fn check_current(&self) -> Result<()> { Ok(()) }
    }
    let directory = Scratch::new();
    let path = directory.0.join("cx.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap(); stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut byte = [0]; assert_eq!(stream.read(&mut byte).unwrap(), 0, "unowned peer must never receive initialize/handshake bytes");
    });
    let process = RefusedPeer(AtomicBool::new(false));
    assert!(connect(&path, &AtomicBool::new(false), Some(&process)).is_err());
    assert!(process.0.load(Ordering::Acquire));
    server.join().unwrap();
}
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::sync::Mutex;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        // macOS TMPDIR plus canonical /private and the UUID can exceed
        // sun_path. These real Unix-socket fixtures require a short base.
        let path = PathBuf::from("/tmp").join(format!(
            "rudder-link-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn peer(text: &str) -> PeerMessage {
    PeerMessage {
        sender_run_id: "peer-one".into(),
        message_id: "mail-one".into(),
        text: text.into(),
    }
}
fn command(id: &str, operation: AgentLinkOperation, expected: Option<&str>) -> AgentLinkCommand {
    AgentLinkCommand {
        command_id: id.into(),
        run_id: "run-one".into(),
        attempt_id: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
        expected_thread_id: "thread-main".into(),
        expected_turn_id: expected.map(str::to_string),
        operation,
        timeout: Duration::from_secs(2),
    }
}
fn queued(command: AgentLinkCommand) -> QueuedCommand {
    let deadline = Instant::now() + command.timeout;
    QueuedCommand { command, deadline }
}
fn lifecycle(thread: &str, turn: &str, status: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":status}}})
}
fn started(turn: &str) -> Value {
    json!({"method":"turn/started","params":{"threadId":"thread-main","turn":{"id":turn,"status":"inProgress"}}})
}

#[test]
fn exact_turn_steer_has_no_new_turn_fallback_on_error() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    let (method, params) = state
        .begin(
            queued(command(
                "steer-one",
                AgentLinkOperation::Steer {
                    message: peer("fix parser"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(method, "turn/steer");
    assert_eq!(params["expectedTurnId"], "turn-one");
    assert_eq!(
        params["additionalContext"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["kind"],
        "untrusted"
    );
    assert!(params["additionalContext"].to_string().contains("peer-one"));
    assert!(!params["input"].to_string().contains("fix parser"));
    state.response(&json!({"id":1,"error":{"code":-32600,"message":"turn ended"}}));
    assert!(state.pending.is_none());
    assert!(matches!(
        &state.output[0],
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Rejected { .. },
            ..
        }
    ));
    assert_eq!(state.output.len(), 1);
}

#[test]
fn interrupt_requires_matching_lifecycle_not_rpc_ack() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    let (method, params) = state
        .begin(
            queued(command(
                "interrupt-one",
                AgentLinkOperation::Interrupt,
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(method, "turn/interrupt");
    assert_eq!(
        params,
        json!({"threadId":"thread-main","turnId":"turn-one"})
    );
    state.response(&json!({"id":1,"result":{}}));
    assert!(state.output.is_empty());
    state.notification(&lifecycle("ephemeral-title", "turn-one", "interrupted"));
    state.notification(&lifecycle("thread-main", "other-turn", "interrupted"));
    state.notification(&json!({"method":"turn/completed","params":{"turn":{"id":"turn-one","status":"interrupted"}}}));
    assert!(state.output.is_empty());
    state.notification(&lifecycle("thread-main", "turn-one", "interrupted"));
    assert!(
        matches!(&state.output[1], AgentLinkEventKind::CommandResult { outcome: AgentLinkOutcome::InterruptConfirmed { turn_id }, .. } if turn_id == "turn-one")
    );
    assert!(state.pending.is_none());
    assert!(state.active_turn.is_none());
}

#[test]
fn interrupt_confirmation_can_precede_rpc_response() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state
        .begin(
            queued(command(
                "interrupt-one",
                AgentLinkOperation::Interrupt,
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&lifecycle("thread-main", "turn-one", "interrupted"));
    state.response(&json!({"id":1,"result":{}}));
    assert_eq!(state.output.len(), 2);
    assert!(matches!(
        &state.output[1],
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::InterruptConfirmed { .. },
            ..
        }
    ));
}

#[test]
fn normal_completion_does_not_claim_interruption_or_erase_new_turn() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state
        .begin(
            queued(command(
                "interrupt-one",
                AgentLinkOperation::Interrupt,
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&started("human-next-turn"));
    state.notification(&lifecycle("thread-main", "turn-one", "completed"));
    assert_eq!(state.active_turn.as_deref(), Some("human-next-turn"));
    assert!(state.last_interrupted.is_none());
    assert!(matches!(
        &state.output[2],
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Rejected { .. },
            ..
        }
    ));
}

#[test]
fn uncertainty_blocks_followup_and_late_evidence_can_reconcile() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    let now = Instant::now();
    state
        .begin(
            queued(command(
                "interrupt-one",
                AgentLinkOperation::Interrupt,
                Some("turn-one"),
            )),
            1,
            now,
        )
        .unwrap();
    state.expire(now + Duration::from_secs(3));
    state.expire(now + Duration::from_secs(4));
    assert_eq!(state.output.len(), 1);
    assert!(matches!(
        &state.output[0],
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Uncertain { .. },
            ..
        }
    ));
    let next = state
        .begin(
            queued(command(
                "new-turn",
                AgentLinkOperation::Deliver {
                    message: peer("continue"),
                },
                None,
            )),
            2,
            now,
        )
        .unwrap();
    assert!(next.is_none());
    state.notification(&lifecycle("thread-main", "turn-one", "interrupted"));
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::InterruptConfirmed { .. },
            ..
        }
    ));
}

#[test]
fn explicit_resume_requires_observed_interrupted_turn_and_never_ordinary_delivery() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state.notification(&lifecycle("thread-main", "turn-one", "interrupted"));
    assert!(state
        .begin(
            queued(command(
                "deliver",
                AgentLinkOperation::Deliver {
                    message: peer("go")
                },
                None
            )),
            1,
            Instant::now()
        )
        .unwrap()
        .is_none());
    assert!(state
        .begin(
            queued(command(
                "bad-resume",
                AgentLinkOperation::Resume {
                    message: peer("go")
                },
                Some("stale-turn")
            )),
            2,
            Instant::now()
        )
        .unwrap()
        .is_none());
    let (method, params) = state
        .begin(
            queued(command(
                "resume",
                AgentLinkOperation::Resume {
                    message: peer("go"),
                },
                Some("turn-one"),
            )),
            3,
            Instant::now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(method, "turn/start");
    assert_eq!(params["threadId"], "thread-main");
    state.response(&json!({"id":3,"result":{"turn":{"id":"next-turn","status":"inProgress"}}}));
    assert_eq!(state.active_turn.as_deref(), Some("next-turn"));
}

#[test]
fn stale_turn_and_idle_interrupt_never_send_controls() {
    for active in [None, Some("another-turn".into())] {
        for operation in [
            AgentLinkOperation::Interrupt,
            AgentLinkOperation::Steer {
                message: peer("go"),
            },
        ] {
            let mut state = Session::new("thread-main".into(), active.clone());
            assert!(state
                .begin(
                    queued(command("control", operation, Some("turn-one"))),
                    1,
                    Instant::now()
                )
                .unwrap()
                .is_none());
            assert!(state.pending.is_none());
        }
    }
}

#[test]
fn duplicate_id_is_not_replayed_and_changed_content_is_rejected() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    let original = command(
        "steer",
        AgentLinkOperation::Steer {
            message: peer("go"),
        },
        Some("turn-one"),
    );
    state
        .begin(queued(original.clone()), 1, Instant::now())
        .unwrap();
    state.response(&json!({"id":1,"result":{"turnId":"turn-one"}}));
    assert!(state
        .begin(queued(original.clone()), 2, Instant::now())
        .unwrap()
        .is_none());
    let mut changed = original.clone();
    changed.operation = AgentLinkOperation::Steer {
        message: peer("different"),
    };
    assert!(state
        .begin(queued(changed), 3, Instant::now())
        .unwrap()
        .is_none());
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Rejected { .. },
            ..
        }
    ));
    assert!(state
        .begin(queued(original), 4, Instant::now())
        .unwrap()
        .is_none());
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::TransportAccepted { .. },
            ..
        }
    ));
}

#[test]
fn completed_fast_turn_before_acceptance_is_not_resurrected() {
    let mut state = Session::new("thread-main".into(), None);
    state
        .begin(
            queued(command(
                "deliver",
                AgentLinkOperation::Deliver {
                    message: peer("go"),
                },
                None,
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&started("fast-turn"));
    state.notification(&lifecycle("thread-main", "fast-turn", "completed"));
    state.response(&json!({"id":1,"result":{"turn":{"id":"fast-turn","status":"inProgress"}}}));
    assert!(state.active_turn.is_none());
    assert!(
        matches!(state.output.last().unwrap(), AgentLinkEventKind::CommandResult { outcome: AgentLinkOutcome::TransportAccepted { turn_id }, .. } if turn_id == "fast-turn")
    );
}

#[test]
fn fast_interrupted_turn_keeps_resume_barrier_after_acceptance() {
    let mut state = Session::new("thread-main".into(), None);
    state
        .begin(
            queued(command(
                "deliver",
                AgentLinkOperation::Deliver {
                    message: peer("go"),
                },
                None,
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&started("fast-turn"));
    state.notification(&lifecycle("thread-main", "fast-turn", "interrupted"));
    state.response(&json!({"id":1,"result":{"turn":{"id":"fast-turn","status":"inProgress"}}}));
    assert!(state.active_turn.is_none());
    assert_eq!(state.last_interrupted.as_deref(), Some("fast-turn"));
}

#[test]
fn new_human_turn_during_start_response_is_uncertain_not_automatic_input_retry() {
    let mut state = Session::new("thread-main".into(), None);
    state
        .begin(
            queued(command(
                "deliver",
                AgentLinkOperation::Deliver {
                    message: peer("go"),
                },
                None,
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&started("human-turn"));
    state.response(&json!({"id":1,"result":{"turn":{"id":"our-turn","status":"inProgress"}}}));
    assert_eq!(state.active_turn.as_deref(), Some("human-turn"));
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Uncertain { .. },
            ..
        }
    ));
    assert!(state.pending.is_some());
}

#[test]
fn malformed_steer_acceptance_is_uncertain_and_has_no_fallback() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state
        .begin(
            queued(command(
                "steer",
                AgentLinkOperation::Steer {
                    message: peer("go"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.response(&json!({"id":1,"result":{"turnId":"other-turn"}}));
    assert!(state.pending.is_some());
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Uncertain { .. },
            ..
        }
    ));
}

#[test]
fn malformed_rpc_error_is_uncertain_and_keeps_control_fenced() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state
        .begin(
            queued(command(
                "steer",
                AgentLinkOperation::Steer {
                    message: peer("go"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.response(&json!({"id":1,"error":null}));
    assert!(state.pending.is_some());
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Uncertain { .. },
            ..
        }
    ));
}

#[test]
fn bounded_expired_or_oversize_input_is_rejected_before_dispatch() {
    let mut state = Session::new("thread-main".into(), None);
    let mut expired = queued(command(
        "expired",
        AgentLinkOperation::Deliver {
            message: peer("go"),
        },
        None,
    ));
    expired.deadline = Instant::now() - Duration::from_millis(1);
    assert!(state.begin(expired, 1, Instant::now()).unwrap().is_none());
    let oversized = command(
        "oversized",
        AgentLinkOperation::Deliver {
            message: peer(&"x".repeat(MAX_MESSAGE + 1)),
        },
        None,
    );
    assert!(validate_command(&oversized).is_err());
    let mut long_timeout = command("timeout", AgentLinkOperation::Interrupt, Some("turn-one"));
    long_timeout.timeout = Duration::from_secs(121);
    assert!(validate_command(&long_timeout).is_err());
}

#[test]
fn prepared_launch_is_private_reversible_argv_data_and_preserves_model_scope() {
    let scratch = Scratch::new();
    let attempt = "51001c4a-c30d-4651-a2ef-a558a9302e5f";
    let original = TerminalCommand::with_args(
        "/usr/bin/codex",
        [
            "-c",
            "mcp_servers.rudder_agents.enabled=true",
            "--sandbox",
            "read-only",
            "-m",
            "test-model",
            "hello $(touch /tmp/should-not-run)",
        ],
    )
    .with_env("RUDDER_FABRIC_ATTEMPT_ID", attempt);
    let mut prepared = original.clone();
    let launch = prepare_codex_in("run-one", attempt, &mut prepared, &scratch.0)
        .unwrap()
        .unwrap();
    assert_eq!(prepared.program, "/bin/sh");
    assert_eq!(&prepared.args[6..], original.args.as_slice());
    assert_eq!(prepared.env, original.env);
    assert!(prepared.args[1].contains("'mcp_servers.rudder_agents.enabled=true' 'app-server'"));
    assert!(prepared.args[1].contains("\"$prog\" --remote \"unix://$sock\" \"$@\""));
    assert_eq!(
        fs::metadata(launch.socket_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!launch.socket_path.exists());
    assert_eq!(
        serde_json::from_value::<AgentLinkLaunch>(serde_json::to_value(&launch).unwrap()).unwrap(),
        launch
    );
}

#[test]
fn no_remote_nesting_or_headless_rewrite_and_attempt_mismatch_rejected() {
    let scratch = Scratch::new();
    let attempt = "51001c4a-c30d-4651-a2ef-a558a9302e5f";
    for args in [
        vec!["exec", "--json", "hello"],
        vec!["--remote", "unix:///tmp/already.sock", "hello"],
    ] {
        let mut command = TerminalCommand::with_args("codex", args);
        let original = command.clone();
        assert!(
            prepare_codex_in("run-one", attempt, &mut command, &scratch.0)
                .unwrap()
                .is_none()
        );
        assert_eq!(command, original);
    }
    let mut command =
        TerminalCommand::new("codex").with_env("RUDDER_FABRIC_ATTEMPT_ID", "different");
    assert!(prepare_codex_in("run-one", attempt, &mut command, &scratch.0).is_err());
}

#[test]
fn managed_claude_launch_is_explicit_private_and_separates_initial_prompt() {
    let scratch = Scratch::new();
    let attempt = "51001c4a-c30d-4651-a2ef-a558a9302e5f";
    let mut command = TerminalCommand::with_args(
        "/private/claude",
        [
            "--permission-mode",
            "default",
            "--mcp-config",
            "/private/mcp.json",
            "the exact authorized task",
        ],
    )
    .with_env("RUDDER_FABRIC_ATTEMPT_ID", attempt);
    let original_env = command.env.clone();
    let launch = prepare_claude_sdk_in(
        "run-one",
        attempt,
        &mut command,
        Some("the exact authorized task"),
        &scratch.0,
        Path::new("/private/rudder"),
    )
    .unwrap();
    assert_eq!(launch.backend, "claude-sdk");
    assert_eq!(command.program, "/private/rudder");
    assert_eq!(command.args[0], "__claude-sdk");
    assert_eq!(command.args[1], "--launch-file");
    assert_eq!(command.env, original_env);
    let spec = Path::new(&command.args[2]);
    assert_eq!(
        fs::metadata(spec).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let spec: Value = serde_json::from_slice(&fs::read(spec).unwrap()).unwrap();
    assert_eq!(spec["initial_prompt"], "the exact authorized task");
    assert_eq!(spec["claude_program"], "/private/claude");
    assert_eq!(
        spec["claude_args"],
        json!([
            "--permission-mode",
            "default",
            "--mcp-config",
            "/private/mcp.json"
        ])
    );
}

#[test]
fn managed_claude_ambiguous_prompt_does_not_rewrite_command() {
    let scratch = Scratch::new();
    let mut command =
        TerminalCommand::with_args("claude", ["--permission-mode", "default", "default"]);
    let original = command.clone();
    assert!(prepare_claude_sdk_in(
        "run-one",
        "51001c4a-c30d-4651-a2ef-a558a9302e5f",
        &mut command,
        Some("default"),
        &scratch.0,
        Path::new("rudder")
    )
    .is_err());
    assert_eq!(command, original);
}

#[test]
fn sdk_steer_requires_observed_interruption_before_accepting_new_turn() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state.replace_steer = true;
    let (method, params) = state
        .begin(
            queued(command(
                "steer",
                AgentLinkOperation::Steer {
                    message: peer("replace plan"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(method, "turn/steer");
    assert_eq!(params["expectedTurnId"], "turn-one");
    state.response(&json!({"id":1,"result":{"turnId":"replacement","replacedTurnId":"turn-one"}}));
    assert!(state.pending.is_some());
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Uncertain { .. },
            ..
        }
    ));
}

#[test]
fn sdk_steer_accepts_replacement_only_after_exact_old_interruption() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state.replace_steer = true;
    state
        .begin(
            queued(command(
                "steer",
                AgentLinkOperation::Steer {
                    message: peer("replace plan"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&lifecycle("thread-main", "turn-one", "interrupted"));
    assert!(state.pending.is_some());
    assert!(!state.output.iter().any(|event| matches!(
        event,
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::InterruptConfirmed { .. },
            ..
        }
    )));
    state.notification(&started("replacement"));
    state.response(&json!({"id":1,"result":{"turnId":"replacement","replacedTurnId":"turn-one"}}));
    assert!(state.pending.is_none());
    assert_eq!(state.active_turn.as_deref(), Some("replacement"));
    assert!(
        matches!(state.output.last().unwrap(), AgentLinkEventKind::CommandResult { outcome: AgentLinkOutcome::TransportAccepted { turn_id }, .. } if turn_id == "replacement")
    );
}

#[test]
fn sdk_normal_completion_rejects_replacement_and_premature_start_stays_uncertain() {
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state.replace_steer = true;
    state
        .begin(
            queued(command(
                "steer",
                AgentLinkOperation::Steer {
                    message: peer("replace plan"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&lifecycle("thread-main", "turn-one", "completed"));
    assert!(matches!(
        state.output.last().unwrap(),
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Rejected { .. },
            ..
        }
    ));
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state.replace_steer = true;
    state
        .begin(
            queued(command(
                "steer",
                AgentLinkOperation::Steer {
                    message: peer("replace plan"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    state.notification(&started("replacement"));
    state.notification(&lifecycle("thread-main", "turn-one", "interrupted"));
    state.response(&json!({"id":1,"result":{"turnId":"replacement","replacedTurnId":"turn-one"}}));
    assert!(state.pending.is_some());
    assert!(
        matches!(
            state.output.last().unwrap(),
            AgentLinkEventKind::CommandResult {
                outcome: AgentLinkOutcome::TransportAccepted { .. },
                ..
            }
        ) == false
    );
}

#[test]
fn sdk_local_command_clears_active_turn_without_claiming_task_completion() {
    let mut state = Session::new("thread-main".into(), Some("local-help".into()));
    state.replace_steer = true;
    state.notification(&lifecycle("thread-main", "local-help", "idle"));
    assert!(state.active_turn.is_none());
    assert!(
        matches!(state.output.last().unwrap(), AgentLinkEventKind::TurnCompleted { status, .. } if status == "idle")
    );
    let mut codex = Session::new("thread-main".into(), Some("turn-one".into()));
    codex.notification(&lifecycle("thread-main", "turn-one", "idle"));
    assert_eq!(codex.active_turn.as_deref(), Some("turn-one"));
    assert!(codex.output.is_empty());
}

#[test]
fn shell_quote_never_expands_substitution_or_apostrophes() {
    let scratch = Scratch::new();
    let proof = scratch.0.join("accidental-expansion");
    let value = format!(
        "a'b `touch {0}` $(touch {0}) \n second line",
        proof.display()
    );
    let result = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("printf '%s' {}", shell_quote(&value))])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap(), value);
    assert!(!proof.exists());
}

#[test]
fn actual_launch_wrapper_shares_private_socket_preserves_argv_and_reaps_server() {
    let scratch = Scratch::new();
    let fake = scratch.0.join("fake codex's cli");
    fs::write(
        &fake,
        r#"#!/usr/bin/python3
import json, os, signal, socket, sys
from pathlib import Path
root = Path(__file__).parent
args = sys.argv[1:]
if 'app-server' in args:
    (root / 'server-argv.json').write_text(json.dumps(args))
    (root / 'server-pid').write_text(str(os.getpid()))
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(args[args.index('--listen') + 1][7:])
    listener.listen(1)
    def stop(signum, frame):
        (root / 'server-reaped').write_text(str(signum))
        listener.close()
        sys.exit(0)
    signal.signal(signal.SIGTERM, stop)
    conn, _ = listener.accept()
    assert conn.recv(64) == b'same-session'
    conn.sendall(b'thread-shared-with-tui')
    conn.close()
    while True: signal.pause()
elif '--remote' in args:
    (root / 'tui-argv.json').write_text(json.dumps(args))
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.connect(args[args.index('--remote') + 1][7:])
    conn.sendall(b'same-session')
    print(conn.recv(128).decode(), flush=True)
    conn.close()
else:
    sys.exit(99)
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let proof = scratch.0.join("must-not-execute");
    let config = format!(
        "mcp.test='apostrophe' $(touch {}) `touch {}`",
        proof.display(),
        proof.display()
    );
    let original = vec![
        "-c".to_string(),
        config.clone(),
        "--sandbox".into(),
        "read-only".into(),
        "hello peer".into(),
    ];
    let mut command = TerminalCommand::with_args(fake.display().to_string(), original.clone());
    let launch = prepare_codex_in(
        "run-one",
        "51001c4a-c30d-4651-a2ef-a558a9302e5f",
        &mut command,
        &scratch.0,
    )
    .unwrap()
    .unwrap();
    let mut process = std::process::Command::new(&command.program)
        .args(&command.args)
        .current_dir(&scratch.0)
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if process.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-(process.id() as i32), libc::SIGKILL);
            }
            let _ = process.wait();
            panic!("fake Codex launch wrapper did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = process.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().trim(),
        "thread-shared-with-tui"
    );
    let server_args: Vec<String> =
        serde_json::from_slice(&fs::read(scratch.0.join("server-argv.json")).unwrap()).unwrap();
    assert_eq!(
        server_args,
        vec![
            "-c".to_string(),
            config,
            "app-server".into(),
            "--listen".into(),
            format!("unix://{}", launch.socket_path.display())
        ]
    );
    let tui_args: Vec<String> =
        serde_json::from_slice(&fs::read(scratch.0.join("tui-argv.json")).unwrap()).unwrap();
    assert_eq!(&tui_args[2..], original.as_slice());
    assert!(
        scratch.0.join("server-reaped").exists(),
        "wrapper must reap its own server when TUI exits"
    );
    assert!(
        !proof.exists(),
        "argv content must never expand in the shell"
    );
}

#[test]
fn thread_binding_requires_cwd_non_ephemeral_root_and_rejects_subagents() {
    let scratch = Scratch::new();
    let root =
        json!({"id":"root","sessionId":"root","cwd":scratch.0,"source":"cli","ephemeral":false});
    assert!(candidate_thread(&root, &scratch.0, None));
    for (key, value) in [
        ("ephemeral", json!(true)),
        ("sessionId", json!("other-root")),
        ("parentThreadId", json!("root-parent")),
        (
            "source",
            json!({"subAgent":{"thread_spawn":{"parent_thread_id":"root"}}}),
        ),
        ("source", json!("subAgentThreadSpawn")),
        ("cwd", json!("/missing/different/repo")),
    ] {
        let mut other = root.clone();
        other[key] = value;
        assert!(
            !candidate_thread(&other, &scratch.0, None),
            "unexpected eligible root: {other}"
        );
    }
    assert!(!candidate_thread(&root, &scratch.0, Some("wrong-thread")));
}

#[test]
fn claude_channel_flag_requires_supported_interactive_profile() {
    assert!(claude_channels_version_supported("2.1.80 (Claude Code)"));
    assert!(claude_channels_version_supported("Claude Code 2.1.290"));
    assert!(!claude_channels_version_supported("2.1.79"));
    assert!(!claude_channels_version_supported("system"));
    let mut command =
        TerminalCommand::with_args("claude", ["--permission-mode", "default", "hello"]);
    configure_claude_channel(&mut command, "rudder_agents", "2.1.290").unwrap();
    assert_eq!(
        &command.args[..3],
        [
            "--dangerously-load-development-channels",
            "server:rudder_agents",
            "--permission-mode"
        ]
    );
    assert!(!command.args.iter().any(|arg| arg.contains("yes")));
    let mut print = TerminalCommand::with_args("claude", ["-p", "hello"]);
    assert!(configure_claude_channel(&mut print, "rudder_agents", "2.1.290").is_err());
}

#[test]
fn claude_interrupt_needs_active_exact_attempt_is_one_shot_and_hook_proof_is_narrow() {
    let now = Instant::now();
    let active = ClaudeActiveOperation {
        run_id: "run-one".into(),
        attempt_id: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
        session_id: "thread-main".into(),
        prompt_id: "turn-one".into(),
        tool_use_id: "tool-one".into(),
        official_state: "working".into(),
        observed_at: now,
    };
    let control = command("interrupt", AgentLinkOperation::Interrupt, Some("turn-one"));
    let mut gate = ClaudeInterruptGate::new(active.run_id.clone(), active.attempt_id.clone());
    assert!(gate
        .authorize(
            &control,
            &ClaudeControlProfile {
                active_ctrl_c_qualified: false
            },
            &active,
            now
        )
        .is_err());
    let profile = ClaudeControlProfile {
        active_ctrl_c_qualified: true,
    };
    let mut idle = active.clone();
    idle.official_state = "input".into();
    assert!(gate.authorize(&control, &profile, &idle, now).is_err());
    assert!(gate
        .authorize(
            &control,
            &profile,
            &active,
            now + Duration::from_millis(251)
        )
        .is_err());
    let mut stale = control.clone();
    stale.attempt_id = "old-attempt".into();
    assert!(gate.authorize(&stale, &profile, &active, now).is_err());
    assert_eq!(
        gate.authorize(&control, &profile, &active, now).unwrap(),
        b"\x03"
    );
    assert!(gate.authorize(&control, &profile, &active, now).is_err());
    let proof = json!({"session_id":"thread-main","hook_event_name":"PostToolUseFailure","tool_use_id":"tool-one","is_interrupt":true});
    assert_eq!(
        gate.observe_hook("old-attempt", &proof),
        ClaudeInterruptObservation::Uncertain
    );
    for field in [
        "is_interrupt",
        "tool_use_id",
        "session_id",
        "hook_event_name",
    ] {
        let mut wrong = proof.clone();
        wrong[field] = Value::Null;
        assert_eq!(
            gate.observe_hook(&active.attempt_id, &wrong),
            ClaudeInterruptObservation::Uncertain
        );
    }
    assert_eq!(
        gate.observe_hook(&active.attempt_id, &proof),
        ClaudeInterruptObservation::OperationInterrupted {
            tool_use_id: "tool-one".into()
        }
    );
    gate.close();
    assert!(gate.authorize(&control, &profile, &active, now).is_err());
}

enum ServerAction {
    Frame(Value),
    Disconnect,
}
struct MockServer {
    _scratch: Scratch,
    launch: AgentLinkLaunch,
    actions: Sender<ServerAction>,
    requests: Receiver<Value>,
    all_requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl MockServer {
    fn new(active: Option<&str>, ambiguous: bool) -> Self {
        let scratch = Scratch::new();
        let launch = AgentLinkLaunch {
            format_version: 1,
            run_id: "run-one".into(),
            attempt_id: "51001c4a-c30d-4651-a2ef-a558a9302e5f".into(),
            backend: "codex".into(),
            socket_path: scratch.0.join("cx.sock"),
            log_path: scratch.0.join("app-server.log"),
        };
        let listener = UnixListener::bind(&launch.socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (actions, action_rx) = mpsc::channel();
        let (request_tx, requests) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let all_requests = Arc::new(Mutex::new(Vec::new()));
        let all = Arc::clone(&all_requests);
        let cwd = scratch.0.clone();
        let turns = active
            .map(|id| vec![json!({"id":id,"status":"inProgress","items":[]})])
            .unwrap_or_default();
        let worker = std::thread::spawn(move || {
            let stream = loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("mock accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            while !stop.load(Ordering::Acquire) {
                while let Ok(action) = action_rx.try_recv() {
                    match action {
                        ServerAction::Frame(value) => {
                            if socket
                                .send(tungstenite::Message::Text(value.to_string()))
                                .is_err()
                            {
                                return;
                            }
                        }
                        ServerAction::Disconnect => return,
                    }
                }
                let message: Value = match socket.read() {
                    Ok(tungstenite::Message::Text(text)) => serde_json::from_str(&text).unwrap(),
                    Ok(tungstenite::Message::Close(_)) => return,
                    Ok(_) => continue,
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(_) => return,
                };
                all.lock().unwrap().push(message.clone());
                let root = |id: &str| json!({"id":id,"sessionId":id,"ephemeral":false,"cwd":cwd,"source":"cli","turns":turns});
                let result = match message["method"].as_str().unwrap_or("") {
                    "initialize" => Some(json!({"userAgent":"rudder_agent_link/0.166.0"})),
                    "initialized" => None,
                    "thread/loaded/list" => Some(
                        json!({"data":if ambiguous {vec!["thread-main", "second-root"]} else {vec!["thread-main"]}}),
                    ),
                    "thread/read" => Some(
                        json!({"thread":root(message["params"]["threadId"].as_str().unwrap())}),
                    ),
                    "thread/resume" => Some(json!({"thread":root("thread-main")})),
                    "thread/turns/list" => Some(json!({"data":turns,"nextCursor":null})),
                    _ => {
                        if request_tx.send(message.clone()).is_err() {
                            return;
                        }
                        None
                    }
                };
                if let Some(result) = result {
                    if socket
                        .send(tungstenite::Message::Text(
                            json!({"id":message["id"],"result":result}).to_string(),
                        ))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
        Self {
            _scratch: scratch,
            launch,
            actions,
            requests,
            all_requests,
            stopped,
            worker: Some(worker),
        }
    }
    fn spawn(&self) -> (AgentLinkHandle, Receiver<AgentLinkEvent>) {
        let (tx, rx) = mpsc::channel();
        (
            spawn_link(self.launch.clone(), &self._scratch.0, None, tx).unwrap(),
            rx,
        )
    }
    fn frame(&self, value: Value) {
        self.actions.send(ServerAction::Frame(value)).unwrap();
    }
    fn next_request(&self) -> Value {
        self.requests
            .recv_timeout(Duration::from_secs(3))
            .expect("expected control RPC")
    }
}
impl Drop for MockServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn until(
    events: &Receiver<AgentLinkEvent>,
    predicate: impl Fn(&AgentLinkEventKind) -> bool,
) -> AgentLinkEvent {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("expected live adapter event");
        assert_eq!(event.run_id, "run-one");
        assert_eq!(event.attempt_id, "51001c4a-c30d-4651-a2ef-a558a9302e5f");
        if predicate(&event.kind) {
            return event;
        }
    }
}

#[test]
fn real_websocket_steer_error_never_starts_substitute_conversation_or_turn() {
    let server = MockServer::new(Some("turn-one"), false);
    let (link, events) = server.spawn();
    until(&events, |event| {
        matches!(event, AgentLinkEventKind::Bound { .. })
    });
    link.submit(command(
        "steer",
        AgentLinkOperation::Steer {
            message: peer("fix parser"),
        },
        Some("turn-one"),
    ))
    .unwrap();
    let request = server.next_request();
    assert_eq!(request["method"], "turn/steer");
    assert_eq!(request["params"]["expectedTurnId"], "turn-one");
    server.frame(json!({"id":request["id"],"error":{"code":-32600,"message":"turn ended"}}));
    until(&events, |event| {
        matches!(
            event,
            AgentLinkEventKind::CommandResult {
                outcome: AgentLinkOutcome::Rejected { .. },
                ..
            }
        )
    });
    assert!(server
        .requests
        .recv_timeout(Duration::from_millis(120))
        .is_err());
    assert!(!server
        .all_requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| matches!(
            request["method"].as_str(),
            Some("thread/start" | "thread/fork" | "turn/start")
        )));
    link.close();
}

#[test]
fn real_websocket_interrupt_ack_timeout_reconciles_only_exact_turn_then_explicit_resume() {
    let server = MockServer::new(Some("turn-one"), false);
    let (link, events) = server.spawn();
    until(&events, |event| {
        matches!(event, AgentLinkEventKind::Bound { .. })
    });
    let mut interrupt = command("interrupt", AgentLinkOperation::Interrupt, Some("turn-one"));
    interrupt.timeout = Duration::from_millis(60);
    link.submit(interrupt).unwrap();
    let request = server.next_request();
    server.frame(json!({"id":request["id"],"result":{}}));
    server.frame(lifecycle("title-thread", "turn-one", "interrupted"));
    until(&events, |event| {
        matches!(
            event,
            AgentLinkEventKind::CommandResult {
                outcome: AgentLinkOutcome::Uncertain { .. },
                ..
            }
        )
    });
    assert!(server
        .requests
        .recv_timeout(Duration::from_millis(70))
        .is_err());
    server.frame(lifecycle("thread-main", "turn-one", "interrupted"));
    until(
        &events,
        |event| matches!(event, AgentLinkEventKind::CommandResult { outcome: AgentLinkOutcome::InterruptConfirmed { turn_id }, .. } if turn_id == "turn-one"),
    );
    link.submit(command(
        "ordinary-delivery",
        AgentLinkOperation::Deliver {
            message: peer("go"),
        },
        None,
    ))
    .unwrap();
    until(
        &events,
        |event| matches!(event, AgentLinkEventKind::CommandResult { command_id, outcome: AgentLinkOutcome::Rejected { .. } } if command_id == "ordinary-delivery"),
    );
    link.submit(command(
        "resume",
        AgentLinkOperation::Resume {
            message: peer("continue with changed plan"),
        },
        Some("turn-one"),
    ))
    .unwrap();
    let resume = server.next_request();
    assert_eq!(resume["method"], "turn/start");
    assert_eq!(resume["params"]["threadId"], "thread-main");
    server.frame(started("turn-two"));
    server.frame(
        json!({"id":resume["id"],"result":{"turn":{"id":"turn-two","status":"inProgress"}}}),
    );
    until(
        &events,
        |event| matches!(event, AgentLinkEventKind::CommandResult { command_id, outcome: AgentLinkOutcome::TransportAccepted { turn_id } } if command_id == "resume" && turn_id == "turn-two"),
    );
    link.close();
    assert!(link
        .submit(command(
            "revive-after-stop",
            AgentLinkOperation::Deliver {
                message: peer("go")
            },
            None
        ))
        .is_err());
}

#[test]
fn real_websocket_disconnect_after_dispatch_is_uncertain_and_closes_attempt() {
    let server = MockServer::new(Some("turn-one"), false);
    let (link, events) = server.spawn();
    until(&events, |event| {
        matches!(event, AgentLinkEventKind::Bound { .. })
    });
    link.submit(command(
        "steer",
        AgentLinkOperation::Steer {
            message: peer("fix parser"),
        },
        Some("turn-one"),
    ))
    .unwrap();
    server.next_request();
    server.actions.send(ServerAction::Disconnect).unwrap();
    until(&events, |event| {
        matches!(
            event,
            AgentLinkEventKind::CommandResult {
                outcome: AgentLinkOutcome::Uncertain { .. },
                ..
            }
        )
    });
    until(&events, |event| {
        matches!(event, AgentLinkEventKind::Closed { .. })
    });
    assert!(link
        .submit(command(
            "retry",
            AgentLinkOperation::Steer {
                message: peer("fix parser")
            },
            Some("turn-one")
        ))
        .is_err());
}

#[test]
fn real_websocket_ambiguous_roots_fail_closed_without_resume_or_model_calls() {
    let server = MockServer::new(Some("turn-one"), true);
    let (_link, events) = server.spawn();
    let closed = until(&events, |event| {
        matches!(event, AgentLinkEventKind::Closed { .. })
    });
    assert!(
        matches!(closed.kind, AgentLinkEventKind::Closed { reason } if reason.contains("ambiguous"))
    );
    assert!(!server
        .all_requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| matches!(
            request["method"].as_str(),
            Some("thread/resume" | "thread/start" | "turn/start")
        )));
}

#[test]
fn sdk_profile_evidence_is_bound_and_handles_capabilities_before_event_pump() {
    let early = json!({"method":"rudder/profile","params":{"threadId":"thread-main","qualified":true,"uncertain":false}});
    assert!(matches!(
        profile_notification(&early, "thread-main"),
        Some(AgentLinkEventKind::ProfileObserved {
            qualified: true,
            uncertain: false
        })
    ));
    assert!(profile_notification(&early, "foreign-thread").is_none());
    let malformed =
        json!({"method":"rudder/profile","params":{"threadId":"thread-main","qualified":true}});
    assert!(matches!(
        profile_notification(&malformed, "thread-main"),
        Some(AgentLinkEventKind::ProfileObserved {
            uncertain: true,
            ..
        })
    ));
}

#[test]
fn codex_peer_control_requires_actual_qualified_server_version_and_has_no_text_fallback() {
    assert!(codex_peer_controls_supported(
        "rudder_agent_link/0.166.0 (Linux; x86_64)"
    ));
    assert!(codex_peer_controls_supported(
        "rudder_agent_link/0.167.0 (macOS; arm64)"
    ));
    for version in [
        "rudder_agent_link/0.165.9",
        "rudder_agent_link/0.166.0-preview",
        "other/0.166.0",
        "unknown",
        "rudder_agent_link/0.166",
    ] {
        assert!(!codex_peer_controls_supported(version));
    }
    let mut state = Session::new("thread-main".into(), Some("turn-one".into()));
    state.controls_qualified = false;
    let result = state
        .begin(
            queued(command(
                "old-server",
                AgentLinkOperation::Steer {
                    message: peer("do not write"),
                },
                Some("turn-one"),
            )),
            1,
            Instant::now(),
        )
        .unwrap();
    assert!(result.is_none());
    assert!(state.pending.is_none());
    assert!(matches!(
        &state.output[0],
        AgentLinkEventKind::CommandResult {
            outcome: AgentLinkOutcome::Unsupported { .. },
            ..
        }
    ));
}
