use rudder_engine::journal::*;
use serde_json::json;
use std::collections::BTreeSet;

fn strings(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| s.to_string()).collect()
}
fn request(key: &str) -> CommandRequest {
    CommandRequest {
        idempotency_key: key.into(),
        target_run: "target".into(),
        target_epoch: 1,
        kind: "steer".into(),
        scope: "project".into(),
        payload: json!({"text":key,"expected_thread_id":"thread-1","expected_turn_id":"turn-1"}),
    }
}

#[test]
fn process_exit_without_sqlite_drop_preserves_committed_intents_and_uncertain_effects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let owner = journal.create_project("p", "engine", "base", 0).unwrap();
    journal.register_run(&owner, "source", 0).unwrap();
    journal.register_run(&owner, "target", 0).unwrap();
    journal.observe_run_attempt(&owner,"target","crash-fixture",0).unwrap();
    journal.observe_runtime(&owner,&RuntimeObservation {
        run_id:"target".into(),run_epoch:1,attempt_id:"crash-fixture".into(),backend:"codex".into(),execution_profile:"codex-app-server".into(),
        thread_id:Some("thread-1".into()),active_turn_id:Some("turn-1".into()),last_interrupted_turn_id:None,
        phase:RuntimePhase::Working,capabilities:strings(&["steer","interrupt"]),pty_owned:true,
    },0).unwrap();
    let grant = CapabilityGrant {
        principal: "source".into(),
        run_id: "source".into(),
        run_epoch: 1,
        operations: strings(&["command:steer"]),
        recipients: [("target".into(), 1)].into_iter().collect(),
        scopes: strings(&["project"]),
        topics: BTreeSet::new(),
        expires_at_ms: None,
    };
    let token = journal.issue_capability(&owner, &grant, 0).unwrap();
    drop(journal);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_writer", "--ignored"])
        .env("RUDDER_JOURNAL_CRASH_PATH", &path)
        .env("RUDDER_JOURNAL_CRASH_TOKEN", token)
        .status()
        .unwrap();
    assert!(status.success());
    let mut reopened = Journal::open(&path).unwrap();
    let recovery = reopened.recover(&owner, 2).unwrap();
    assert_eq!(recovery.pending.len(), 1);
    assert_eq!(recovery.uncertain.len(), 1);
    assert!(reopened
        .claim_effect(&owner, &recovery.uncertain[0].command_id, 3)
        .unwrap()
        .is_none());
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        db.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
}

#[test]
#[ignore = "subprocess fixture; deliberately exits without running destructors"]
fn crash_writer() {
    let path = std::env::var_os("RUDDER_JOURNAL_CRASH_PATH").expect("subprocess path");
    let token = std::env::var("RUDDER_JOURNAL_CRASH_TOKEN").expect("subprocess capability");
    let mut journal = Journal::open(std::path::PathBuf::from(path)).unwrap();
    let owner = journal.project_state("p").unwrap().owner;
    let first = journal
        .accept_command(&token, &request("started"), 1)
        .unwrap();
    journal
        .claim_effect(&owner, &first.command_id, 1)
        .unwrap()
        .unwrap();
    journal
        .accept_command(&token, &request("pending"), 1)
        .unwrap();
    std::process::exit(0);
}
