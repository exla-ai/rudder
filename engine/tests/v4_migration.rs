//! Actual SQLite/OS-lock migration drills; recovery files are disposable fixtures.
#[path = "../../native/src/v4_migration/mod.rs"]
mod migration;
use migration::*;
use rudder_engine::journal::Journal;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn quiet() -> Quiescence {
    Quiescence {
        scheduler_shutdown_confirmed: true,
        effects_reconciled: true,
        launchers_disabled: true,
        external_credentials_fenced: true,
    }
}
fn bundle(root: &Path, name: &str, contents: &[u8]) -> (PathBuf, String) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("objects")).unwrap();
    let blob = |bytes: &[u8]| {
        let hash = hex::encode(Sha256::digest(bytes));
        fs::write(dir.join("objects").join(&hash), bytes).unwrap();
        json!({"kind":"blob","algorithm":"sha256","digest":hash,"size_bytes":bytes.len()})
    };
    let object = blob(contents);
    let roots = json!([
        {"name":"integration-source","path":"/legacy","role":"source"},
        {"name":"git-history","path":"/legacy/.git","role":"history"},
        {"name":"jj-history","path":"/legacy/.jj","role":"history"},
        {"name":"legacy-runs","path":"/legacy/.rudder/runs","role":"legacy-state"},
        {"name":"session-0","path":"/legacy/session.jsonl","role":"sessions","run_id":"stopped"}
    ]);
    let mut entries = Vec::new();
    for name in [
        "integration-source",
        "git-history",
        "jj-history",
        "legacy-runs",
    ] {
        entries.push(json!({"root":name,"path":"","type":"directory","mode":493}));
        if name != "legacy-runs" {
            entries.push(
                json!({"root":name,"path":"retained.txt","type":"file","mode":420,"object":object}),
            );
        }
    }
    for (id, status) in [("completed", "completed"), ("stopped", "cancelled")] {
        let raw = json!({"id":id,"status":status,"worktree":{"path":"/legacy/workspace","enabled":true},"session":{"nativeSessionId":"session-1"}});
        // Both historical runs retain the same shared transcript in this fixture.
        entries.push(json!({"root":"legacy-runs","path":id,"type":"directory","mode":493}));
        entries.push(json!({"root":"legacy-runs","path":format!("{id}/run.json"),"type":"file","mode":420,"object":blob(&serde_json::to_vec(&raw).unwrap())}));
    }
    let mut roots = roots.as_array().unwrap().clone();
    roots.push(json!({"name":"session-1","path":"/legacy/completed.jsonl","role":"sessions","run_id":"completed"}));
    for name in ["session-0", "session-1"] {
        entries.push(json!({"root":name,"path":"","type":"file","mode":384,"object":object}));
    }
    let manifest = json!({"format_version":1,"kind":"v4_legacy_recovery","jj_version":"0.40.0","project_id":"project","migration_id":"migration","roots":roots,"entries":entries});
    let bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(dir.join("manifest.json"), &bytes).unwrap();
    (dir, hex::encode(Sha256::digest(bytes)))
}
struct Fixture {
    dir: TempDir,
    repo: PathBuf,
    database: PathBuf,
    request: MigrationRequest,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let database = dir.path().join("journal.sqlite");
        let mut journal = Journal::open(&database).unwrap();
        let owner = journal
            .create_project("project", "legacy", "accepted-head", 1)
            .unwrap();
        journal.register_run(&owner, "completed", 1).unwrap();
        let (archive, hash) = bundle(
            dir.path(),
            "legacy",
            b"source history sessions original user edits",
        );
        let request = MigrationRequest {
            format_version: 1,
            migration_id: "migration".into(),
            repository: repo.clone(),
            previous_authority: "legacy-native".into(),
            previous_owner: owner,
            next_owner_id: "fabric-migration".into(),
            runs: vec![
                LegacyRun {
                    run_id: "completed".into(),
                    expected_epoch: Some(1),
                },
                LegacyRun {
                    run_id: "stopped".into(),
                    expected_epoch: None,
                },
            ],
            recovery_bundle: archive,
            recovery_manifest_sha256: hash,
        };
        Self {
            dir,
            repo,
            database,
            request,
        }
    }
    fn session(&self) -> MigrationSession {
        MigrationSession::open(&self.repo, &quiet()).unwrap()
    }
    fn journal(&self) -> Journal {
        Journal::open(&self.database).unwrap()
    }
    fn prepare(&self) {
        self.session()
            .prepare(&self.journal(), self.request.clone())
            .unwrap();
    }
    fn advance(&self) -> MigrationReceipt {
        self.session()
            .advance(&mut self.journal(), "migration", 10)
            .unwrap()
    }
    fn until(&self, phase: Phase) -> MigrationReceipt {
        for _ in 0..16 {
            let receipt = self.advance();
            if receipt.phase == phase {
                return receipt;
            }
        }
        panic!("phase not reached");
    }
}

#[test]
fn restart_at_every_committed_phase_preserves_history_and_never_starts_work() {
    let f = Fixture::new();
    f.prepare();
    for phase in [
        Phase::Exported,
        Phase::Importing,
        Phase::Importing,
        Phase::Importing,
        Phase::Imported,
        Phase::Transferring,
        Phase::Committed,
    ] {
        let receipt = f.advance();
        assert_eq!(receipt.phase, phase);
        // Every call reopens SQLite and reacquires the real owner lock.
        assert_eq!(f.session().read().unwrap().unwrap(), receipt);
    }
    let j = f.journal();
    let receipt = f.session().read().unwrap().unwrap();
    assert_eq!(receipt.owner.epoch, 2);
    assert_eq!(receipt.run_epochs["stopped"], 2);
    assert_eq!(
        j.project_state("project").unwrap().integration_head,
        "accepted-head"
    );
    for run in ["completed", "stopped"] {
        assert!(j
            .runtime_snapshot(&receipt.owner, run, 20)
            .unwrap()
            .is_none());
        assert!(j.current_run_attempt(&receipt.owner, run, 2).is_err());
    }
    assert_eq!(f.session().prepare(&j, f.request.clone()).unwrap(), receipt);
    assert!(refuse_implicit_startup(&f.repo).is_err());
    assert!(f.request.recovery_bundle.join("manifest.json").exists());
}
#[test]
fn imports_and_owner_transfer_committed_before_file_receipt_are_reconciled_once() {
    let f = Fixture::new();
    f.prepare();
    f.until(Phase::Importing);
    f.advance();
    let mut j = f.journal();
    j.register_run(&f.request.previous_owner, "stopped", 11)
        .unwrap();
    // Simulate process death after SQLite commit and before receipt save.
    let receipt = f.advance();
    assert_eq!(receipt.imported, 2);
    assert_eq!(j.run_epoch("project", "stopped").unwrap(), 1);
    f.until(Phase::Transferring);
    let next = j
        .transfer_owner(&f.request.previous_owner, "fabric-migration", 12)
        .unwrap();
    let receipt = f.advance();
    assert_eq!(receipt.phase, Phase::Committed);
    assert_eq!(receipt.owner, next);
    assert_eq!(j.run_epoch("project", "stopped").unwrap(), 2);
    assert!(j
        .register_run(&f.request.previous_owner, "duplicate", 13)
        .is_err());
}
#[test]
fn rollback_after_partial_import_and_after_commit_retains_both_exports() {
    for committed in [false, true] {
        let f = Fixture::new();
        f.prepare();
        if committed {
            f.until(Phase::Committed);
        } else {
            f.until(Phase::Importing);
            f.advance();
            f.advance();
        }
        let session = f.session();
        let mut j = f.journal();
        let paused = session.pause_for_rollback(&mut j, "migration", 20).unwrap();
        assert_eq!(paused.phase, Phase::RollbackPaused);
        assert!(!j.project_state("project").unwrap().owner_active);
        assert!(session.advance(&mut j, "migration", 21).is_err());
        assert!(session
            .finish_rollback(
                &j,
                "migration",
                &f.request.recovery_bundle,
                &f.request.recovery_manifest_sha256
            )
            .is_err());
        let (delta, hash) = bundle(
            f.dir.path(),
            "post-pause-deltas",
            b"new source deltas and sessions",
        );
        assert!(session
            .finish_rollback(&j, "migration", &delta, &hash)
            .is_err());
        let manifest_path = delta.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["rollback_checkpoint"] = json!(paused.rollback_checkpoint);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(manifest_path, &bytes).unwrap();
        let hash = hex::encode(Sha256::digest(bytes));
        let rolled = session
            .finish_rollback(&j, "migration", &delta, &hash)
            .unwrap();
        assert_eq!(rolled.phase, Phase::RolledBack);
        assert_eq!(
            session
                .finish_rollback(&j, "migration", &delta, &hash)
                .unwrap(),
            rolled
        );
        assert_eq!(
            j.project_state("project").unwrap().integration_head,
            "accepted-head"
        );
        assert!(f.request.recovery_bundle.exists() && delta.exists());
        assert!(refuse_implicit_startup(&f.repo).is_err());
    }
}
#[test]
fn interrupted_rollback_release_reconciles_original_epoch() {
    let f = Fixture::new();
    f.prepare();
    let committed = f.until(Phase::Committed);
    // Save the durable pre-effect rollback intent exactly as a crashed host would.
    let path = f.repo.join(".rudder/engine/v4-migration.json");
    let mut intent = committed.clone();
    intent.phase = Phase::RollbackPausing;
    fs::write(path, serde_json::to_vec(&intent).unwrap()).unwrap();
    f.journal().release_owner(&committed.owner, 20).unwrap();
    let paused = f
        .session()
        .pause_for_rollback(&mut f.journal(), "migration", 21)
        .unwrap();
    assert_eq!(paused.phase, Phase::RollbackPaused);
    assert_eq!(paused.owner.epoch, committed.owner.epoch + 1);
}
#[test]
fn concurrent_and_stale_actors_are_refused_without_taking_over() {
    let f = Fixture::new();
    {
        let _session = f.session();
        assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
    }
    f.prepare();
    let mut conflicting = f.request.clone();
    conflicting.next_owner_id = "other".into();
    assert!(f.session().prepare(&f.journal(), conflicting).is_err());
    f.journal()
        .transfer_owner(&f.request.previous_owner, "other-owner", 20)
        .unwrap();
    assert!(f
        .session()
        .advance(&mut f.journal(), "migration", 21)
        .is_err());
    assert!(f
        .session()
        .pause_for_rollback(&mut f.journal(), "migration", 21)
        .is_err());
    assert_eq!(
        f.journal().project_state("project").unwrap().owner.owner_id,
        "other-owner"
    );
}
#[test]
fn stale_run_inventory_and_corrupt_archives_fail_closed() {
    let f = Fixture::new();
    let mut stale = f.request.clone();
    stale.runs[0].expected_epoch = Some(2);
    assert!(f.session().prepare(&f.journal(), stale).is_err());
    f.prepare();
    f.advance();
    f.journal()
        .advance_run_epoch(&f.request.previous_owner, "completed", 1, 5)
        .unwrap();
    assert!(f
        .session()
        .advance(&mut f.journal(), "migration", 10)
        .is_err());
    let corrupt = Fixture::new();
    let object = fs::read_dir(corrupt.request.recovery_bundle.join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(object, b"corrupted").unwrap();
    assert!(corrupt
        .session()
        .prepare(&corrupt.journal(), corrupt.request.clone())
        .is_err());
}
#[test]
fn known_live_legacy_processes_even_terminal_runs_are_never_migrated() {
    for name in ["pid", "controllerPid", "backendPid"] {
        let f = Fixture::new();
        let runs = f.repo.join(".rudder/runs/terminal");
        fs::create_dir_all(&runs).unwrap();
        fs::write(
            runs.join("run.json"),
            serde_json::to_vec(&json!({"status":"completed","process":{name:std::process::id()}}))
                .unwrap(),
        )
        .unwrap();
        assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
    }
    for file in [".rudder/daemon.pid", ".rudder/engine/owner.json"] {
        let f = Fixture::new();
        let file = f.repo.join(file);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            file,
            serde_json::to_vec(&json!({"pid":std::process::id()})).unwrap(),
        )
        .unwrap();
        assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
    }
    let f = Fixture::new();
    let mut q = quiet();
    q.external_credentials_fenced = false;
    assert!(MigrationSession::open(&f.repo, &q).is_err());
}
#[test]
fn malformed_or_linked_legacy_metadata_refuses_quiescence() {
    let f = Fixture::new();
    fs::create_dir_all(f.repo.join(".rudder")).unwrap();
    fs::write(f.repo.join(".rudder/daemon.pid"), b"broken").unwrap();
    assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
    fs::remove_file(f.repo.join(".rudder/daemon.pid")).unwrap();
    std::os::unix::fs::symlink(f.dir.path(), f.repo.join(".rudder/runs")).unwrap();
    assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
}

#[test]
fn changed_v4_work_requires_explicit_current_owner_and_run_epochs_for_rollback() {
    let f = Fixture::new();
    f.prepare();
    let committed = f.until(Phase::Committed);
    let mut j = f.journal();
    j.advance_run_epoch(&committed.owner, "stopped", 2, 15)
        .unwrap();
    let session = f.session();
    assert!(session.pause_for_rollback(&mut j, "migration", 20).is_err());
    assert!(session
        .pause_for_rollback_at(
            &mut j,
            "migration",
            &committed.owner,
            &committed.run_epochs,
            20
        )
        .is_err());
    let expected = j
        .owned_runs(&committed.owner)
        .unwrap()
        .into_iter()
        .map(|r| (r.run_id, r.run_epoch))
        .collect();
    let paused = session
        .pause_for_rollback_at(&mut j, "migration", &committed.owner, &expected, 20)
        .unwrap();
    assert_eq!(paused.phase, Phase::RollbackPaused);
    assert_eq!(paused.run_epochs["stopped"], 4);
    let newer = j.transfer_owner(&paused.owner, "newer-owner", 21).unwrap();
    assert!(session.pause_for_rollback(&mut j, "migration", 22).is_err());
    assert_eq!(j.project_state("project").unwrap().owner, newer);
}

#[test]
fn owner_lock_is_an_actual_engine_lock_and_not_a_migration_flag() {
    use fs2::FileExt;
    let f = Fixture::new();
    fs::create_dir_all(f.repo.join(".rudder/engine")).unwrap();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.repo.join(".rudder/engine/owner.lock"))
        .unwrap();
    file.lock_exclusive().unwrap();
    assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
    drop(file);
    assert!(MigrationSession::open(&f.repo, &quiet()).is_ok());
}

#[test]
fn bare_legacy_daemon_pid_and_unknown_process_metadata_are_refused() {
    let f = Fixture::new();
    fs::create_dir_all(f.repo.join(".rudder/runs/run")).unwrap();
    fs::write(
        f.repo.join(".rudder/daemon.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
    fs::remove_file(f.repo.join(".rudder/daemon.pid")).unwrap();
    fs::write(
        f.repo.join(".rudder/runs/run/run.json"),
        br#"{"id":"run","status":"paused","process":"unknown"}"#,
    )
    .unwrap();
    assert!(MigrationSession::open(&f.repo, &quiet()).is_err());
}
