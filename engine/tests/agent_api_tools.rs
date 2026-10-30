use rudder_engine::agent_api::*;
use rudder_engine::journal::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}
struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    owner: OwnerFence,
    a_file: PathBuf,
    b_file: PathBuf,
    a_token: String,
}
fn grant(run: &str) -> CapabilityGrant {
    CapabilityGrant {
        principal: format!("peer-{run}"),
        run_id: run.into(),
        run_epoch: 1,
        operations: set(&[
            "agents:find",
            "message:send",
            "mailbox:read",
            "mailbox:ack",
            "command:steer",
            "command:interrupt",
            "command:resume",
            "topics:read",
        ]),
        recipients: [("a".into(), 1), ("b".into(), 1)].into_iter().collect(),
        scopes: set(&["project"]),
        topics: set(&["api"]),
        expires_at_ms: None,
    }
}
fn write_identity(path: &Path, identity: &AgentIdentityFile) {
    let bytes = serde_json::to_vec(identity).unwrap();
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
fn identity(run: &str, token: &str) -> AgentIdentityFile {
    AgentIdentityFile {
        format_version: 1,
        project_id: "p".into(),
        run_id: run.into(),
        run_epoch: 1,
        owner_epoch: 1,
        token: token.into(),
    }
}
fn working_runtime(journal: &mut Journal, owner: &OwnerFence) {
    let now=now_ms().unwrap();
    let attempt_id="api-fixture-b".to_string();
    let run_epoch=journal.observe_run_attempt(owner, "b", &attempt_id, now).unwrap();
    journal.observe_runtime(owner,&RuntimeObservation {
        run_id:"b".into(),run_epoch,attempt_id,backend:"codex".into(),execution_profile:"codex-app-server".into(),
        thread_id:Some("thread-1".into()),active_turn_id:Some("turn-1".into()),last_interrupted_turn_id:None,
        phase:RuntimePhase::Working,capabilities:set(&["inspect","steer","interrupt"]),pty_owned:true,
    },now).unwrap();
}
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let mut j = Journal::open(&path).unwrap();
    let owner = j.create_project("p", "engine", "base", 0).unwrap();
    for run in ["a", "b", "hidden"] {
        j.register_run(&owner, run, 0).unwrap();
    }
    working_runtime(&mut j, &owner);
    let a_token = j.issue_capability(&owner, &grant("a"), 0).unwrap();
    let b_token = j.issue_capability(&owner, &grant("b"), 0).unwrap();
    let a_file = dir.path().join("a.json");
    let b_file = dir.path().join("b.json");
    write_identity(&a_file, &identity("a", &a_token));
    write_identity(&b_file, &identity("b", &b_token));
    Fixture {
        _dir: dir,
        path,
        owner,
        a_file,
        b_file,
        a_token,
    }
}
fn send(key: &str) -> Value {
    json!({"idempotency_key":key,"recipient_run":"b","recipient_epoch":1,"scope":"project","message":"hello λ 🦀\npeer context"})
}

#[test]
fn tools_preserve_utf8_authorship_idempotency_and_explicit_receipt() {
    let f = fixture();
    let a = AgentApi::open(&f.path, &f.a_file).unwrap();
    let b = AgentApi::open(&f.path, &f.b_file).unwrap();
    let found = a.call("agents_find", json!({})).unwrap();
    assert_eq!(found["agents"].as_array().unwrap().len(), 2);
    let first = a.call("agents_send", send("one")).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        a.call("agents_send", send("one")).unwrap(),
        first,
        "auto TTL must not change retry body"
    );
    assert_eq!(first["message"]["author"], "peer-a");
    assert_eq!(
        first["message"]["request"]["body"]["text"],
        "hello λ 🦀\npeer context"
    );
    let mut impersonate = send("fake");
    impersonate["author"] = json!("user");
    assert!(a.call("agents_send", impersonate).is_err());
    let mut denied = send("denied");
    denied["recipient_run"] = json!("hidden");
    assert!(a.call("agents_send", denied).is_err());
    let pending = b.pending_messages().unwrap();
    assert_eq!(pending.len(), 1);
    let notification = channel_notification(&pending[0]).unwrap();
    let encoded = serde_json::to_string(&notification).unwrap();
    assert!(!encoded.contains('\n'));
    assert!(encoded.contains("🦀"));
    assert_eq!(notification["method"], "notifications/claude/channel");
    assert_eq!(notification["params"]["meta"]["authority"], "peer");
    assert_eq!(
        b.call("agents_inbox", json!({})).unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "writing/reading a channel envelope must not ack"
    );
    b.call(
        "agents_ack",
        json!({"message_id":first["message"]["message_id"]}),
    )
    .unwrap();
    assert!(b.pending_messages().unwrap().is_empty());
}

#[test]
fn identity_files_are_pinned_and_same_attempt_token_rotation_is_supported() {
    let f = fixture();
    let api = AgentApi::open(&f.path, &f.a_file).unwrap();
    let mut j = Journal::open(&f.path).unwrap();
    let rotated = j.issue_capability(&f.owner, &grant("a"), 0).unwrap();
    j.revoke_capability(&f.owner, &f.a_token, 0).unwrap();
    write_identity(&f.a_file, &identity("a", &rotated));
    assert!(api.call("agents_find", json!({})).is_ok());
    let mut wrong = identity("a", &rotated);
    wrong.project_id = "other".into();
    write_identity(&f.a_file, &wrong);
    assert!(api.call("agents_find", json!({})).is_err());
    let mut wrong = identity("b", &rotated);
    write_identity(&f.a_file, &wrong);
    assert!(
        AgentApi::open(&f.path, &f.a_file).is_err(),
        "token cannot assert a different sender"
    );
    wrong = identity("a", &rotated);
    wrong.owner_epoch = 2;
    write_identity(&f.a_file, &wrong);
    assert!(AgentApi::open(&f.path, &f.a_file).is_err());
    j.advance_run_epoch(&f.owner, "a", 1, 0).unwrap();
    let mut new = grant("a");
    new.run_epoch = 2;
    new.recipients.insert("a".into(), 2);
    let token = j.issue_capability(&f.owner, &new, 0).unwrap();
    let mut next = identity("a", &token);
    next.run_epoch = 2;
    write_identity(&f.a_file, &next);
    assert!(
        api.call("agents_find", json!({})).is_err(),
        "old MCP session must not adopt replacement's credentials"
    );
    assert!(AgentApi::open(&f.path, &f.a_file).is_ok());
    assert!(!format!("{:?}", next).contains(&token));
}

#[test]
fn stale_identity_revoked_tokens_unprotected_files_and_symlinks_fail_closed() {
    let f = fixture();
    let api = AgentApi::open(&f.path, &f.a_file).unwrap();
    let mut j = Journal::open(&f.path).unwrap();
    j.revoke_capability(&f.owner, &f.a_token, 0).unwrap();
    assert!(api.call("agents_find", json!({})).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f.b_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(AgentApi::open(&f.path, &f.b_file).is_err());
        std::fs::set_permissions(&f.b_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = f.b_file.with_extension("link");
        std::os::unix::fs::symlink(&f.b_file, &link).unwrap();
        assert!(AgentApi::open(&f.path, &link).is_err());
    }
    let next = j.transfer_owner(&f.owner, "new-owner", 0).unwrap();
    assert!(next.epoch > f.owner.epoch);
    assert!(AgentApi::open(&f.path, &f.b_file).is_err());
}

#[test]
fn ask_reply_wait_is_asynchronous_correlated_and_does_not_ack() {
    let f = fixture();
    let a = AgentApi::open(&f.path, &f.a_file).unwrap();
    let b = AgentApi::open(&f.path, &f.b_file).unwrap();
    let question = a.call("agents_ask", send("question")).unwrap();
    let id = question["message"]["message_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ready = id.clone();
    let reply = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        b.call(
            "agents_reply",
            json!({"idempotency_key":"reply","reply_to":ready,"message":"answer ✓"}),
        )
        .unwrap()
    });
    let started = Instant::now();
    let received = a
        .call("agents_await", json!({"timeout_ms":1000,"reply_to":id}))
        .unwrap();
    assert!(!received["timed_out"].as_bool().unwrap());
    assert!(started.elapsed() < Duration::from_millis(600));
    assert_eq!(
        received["messages"][0]["request"]["body"]["text"],
        "answer ✓"
    );
    let sent = reply.join().unwrap();
    assert_eq!(
        sent["message"]["correlation_id"],
        question["message"]["correlation_id"]
    );
    assert_eq!(a.pending_messages().unwrap().len(), 1);
}

#[test]
fn wait_deadline_is_bounded_even_under_sqlite_contention_and_can_be_cancelled() {
    let f = fixture();
    let a = AgentApi::open(&f.path, &f.a_file).unwrap();
    let started = Instant::now();
    assert_eq!(
        a.call("agents_await", json!({"timeout_ms":60})).unwrap()["timed_out"],
        true
    );
    assert!(started.elapsed() < Duration::from_millis(400));
    let db = rusqlite::Connection::open(&f.path).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    assert_eq!(
        a.call("agents_await", json!({"timeout_ms":60})).unwrap()["timed_out"],
        true
    );
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "wait must not inherit journal's normal five second busy timeout"
    );
    db.execute_batch("ROLLBACK").unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let notify = cancelled.clone();
    let worker = std::thread::spawn(move || {
        a.call_with_cancel("agents_await", json!({"timeout_ms":30000}), &notify)
    });
    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    cancelled.store(true, Ordering::Relaxed);
    assert!(worker.join().unwrap().is_err());
    assert!(started.elapsed() < Duration::from_millis(400));
}

#[test]
fn active_controls_are_durable_intent_and_status_is_sender_scoped() {
    let f = fixture();
    let a = AgentApi::open(&f.path, &f.a_file).unwrap();
    let b = AgentApi::open(&f.path, &f.b_file).unwrap();
    let request = json!({"idempotency_key":"stop-turn","target_run":"b","target_epoch":1,"scope":"project","reason":"peer needs to rebase","expected_thread_id":"thread-1","expected_turn_id":"turn-1"});
    let accepted = a.call("agents_interrupt", request.clone()).unwrap();
    assert_eq!(accepted["execution_observed"], false);
    assert_eq!(accepted["receipt"]["state"], "accepted");
    assert_eq!(a.call("agents_interrupt", request).unwrap(), accepted);
    let id = &accepted["receipt"]["command_id"];
    assert!(b.call("agents_status", json!({"command_id":id})).is_err());
    assert_eq!(
        a.call("agents_status", json!({"command_id":id})).unwrap()["receipt"],
        accepted["receipt"]
    );
    let pending = a
        .call("agents_await", json!({"command_id":id,"timeout_ms":0}))
        .unwrap();
    assert_eq!(pending["timed_out"], true);
    let mut j = Journal::open(&f.path).unwrap();
    let claim = j
        .claim_effect(&f.owner, id.as_str().unwrap(), now_ms().unwrap())
        .unwrap()
        .unwrap();
    j.finish_effect(
        &claim,
        &json!({"status":"failed","reason":"backend profile has not qualified interruption"}),
        now_ms().unwrap(),
    )
    .unwrap();
    let outcome = a
        .call("agents_await", json!({"command_id":id,"timeout_ms":1000}))
        .unwrap();
    assert_eq!(outcome["receipt"]["result"]["status"], "failed");
}

#[test]
fn api_ack_holes_and_bad_arguments_cannot_skip_delivery_or_change_identity() {
    let f = fixture();
    let a = AgentApi::open(&f.path, &f.a_file).unwrap();
    let b = AgentApi::open(&f.path, &f.b_file).unwrap();
    let first = a.call("agents_send", send("first")).unwrap();
    let second = a.call("agents_send", send("second")).unwrap();
    let ack = b
        .call(
            "agents_ack",
            json!({"message_id":second["message"]["message_id"]}),
        )
        .unwrap();
    assert_eq!(ack["cursor"]["sequence"], 0);
    b.call(
        "agents_ack",
        json!({"message_id":first["message"]["message_id"]}),
    )
    .unwrap();
    assert_eq!(
        b.call("agents_inbox", json!({})).unwrap()["cursor"]["sequence"],
        second["message"]["sequence"]
    );
    for (tool, args) in [
        ("agents_find", json!({"identity_file":"another"})),
        ("agents_await", json!({"timeout_ms":30001})),
        ("agents_inbox", json!({"limit":257})),
        ("agents_find", json!([])),
        ("agents_mint", json!({})),
    ] {
        assert!(a.call(tool, args).is_err());
    }
    let mut oversized = send("oversized");
    oversized["message"] = Value::String("x".repeat(129 * 1024));
    assert!(a.call("agents_send", oversized).is_err());
    let defs = tool_definitions();
    assert_eq!(defs.as_array().unwrap().len(), 12);
    assert!(!defs.to_string().contains("identity_file"));
}

#[test]
fn simultaneous_first_message_retries_choose_one_durable_expiry() {
    let f = fixture();
    let a = AgentApi::open(&f.path, &f.a_file).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let api = a.clone();
            let ready = barrier.clone();
            std::thread::spawn(move || {
                ready.wait();
                api.call("agents_send", send("concurrent-first"))
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().unwrap().unwrap())
        .collect();
    assert_eq!(results[0], results[1]);
}

/// Node protocol tests seed data through the real Rust API rather than copying
/// SQLite schemas or bypassing capability issuance with ad-hoc SQL.
#[test]
#[ignore = "fixture exporter invoked by agent-mcp Node integration tests"]
fn export_agent_mcp_fixture() {
    let root = PathBuf::from(
        std::env::var_os("RUDDER_AGENT_MCP_FIXTURE_DIR").expect("fixture output directory"),
    );
    let paths = rudder_engine::paths::FabricPaths::new(&root);
    std::fs::create_dir_all(&paths.root).unwrap();
    let mut j = Journal::open(&paths.journal).unwrap();
    let owner = j.create_project("p", "engine", "base", 0).unwrap();
    for run in ["a", "b", "hidden"] {
        j.register_run(&owner, run, 0).unwrap();
    }
    working_runtime(&mut j, &owner);
    let a = j.issue_capability(&owner, &grant("a"), 0).unwrap();
    let b = j.issue_capability(&owner, &grant("b"), 0).unwrap();
    let a_file = paths.root.join("a.json");
    let b_file = paths.root.join("b.json");
    write_identity(&a_file, &identity("a", &a));
    write_identity(&b_file, &identity("b", &b));
    std::fs::write(
        root.join("fixture.json"),
        serde_json::to_vec(&json!({"journal":paths.journal,"a":a_file,"b":b_file})).unwrap(),
    )
    .unwrap();
}
