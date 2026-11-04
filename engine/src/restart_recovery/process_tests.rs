use super::super::tests::{fixture, request, Fixture};
use super::*;
use crate::journal::Reconciliation;
use crate::{
    protocol::{CheckpointConsistency, Durability},
    store::{CheckpointManifest, LocalStore, SourceManifest},
};
use serde_json::json;
use std::{
    cell::RefCell,
    fs,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Instant,
};

struct Resources {
    live: PathBuf,
    store: LocalStore,
    checkpoint_calls: Arc<AtomicUsize>,
    retire_calls: Arc<AtomicUsize>,
    retired: Arc<AtomicBool>,
    fail_checkpoint: bool,
    fail_retirement_once: bool,
    cancel_after_retirement: bool,
}
impl RuntimeRetirement for Resources {
    fn checkpoint(&mut self, id: &str, original: &FinalizationBudget) -> Result<ObjectRef> {
        original.remaining()?;
        self.checkpoint_calls.fetch_add(1, Ordering::SeqCst);
        ensure!(!self.fail_checkpoint, "deterministic checkpoint failure");
        // The sole child is blocked on stdin and has no source writes. Import
        // real source bytes and pin a real checkpoint closure before exposure.
        let _staging = self.store.begin_staging(Duration::from_secs(60))?;
        let directory = self
            .store
            .import_tree(&self.live, CheckpointConsistency::Quiesced)?;
        let source = self.store.put_source(&SourceManifest {
            format_version: 1,
            directory,
            parents: vec![],
            consistency: CheckpointConsistency::Quiesced,
        })?;
        let checkpoint = self.store.put_checkpoint(&CheckpointManifest {
            format_version: 1,
            source,
            environment: None,
            parents: vec![],
            consistency: CheckpointConsistency::Quiesced,
            durability: Durability::Local,
            knowledge_cursor: 0,
        })?;
        self.store.pin(id, &checkpoint)?;
        original.remaining()?;
        Ok(checkpoint)
    }
    fn retire(&mut self, _id: &str, original: &FinalizationBudget) -> Result<()> {
        original.remaining()?;
        self.retire_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_retirement_once {
            self.fail_retirement_once = false;
            anyhow::bail!("deterministic resource retirement failure before deletion");
        }
        fs::remove_dir_all(&self.live)?;
        self.retired.store(true, Ordering::SeqCst);
        if self.cancel_after_retirement {
            original.cancel();
        }
        Ok(())
    }
}

fn owned_runtime(
    f: &Fixture,
    workers: Vec<OwnedWorkerJoin>,
    original: FinalizationBudget,
) -> (LocalRuntimeDrain<Resources>, PathBuf) {
    let live = f._dir.path().join("live");
    fs::create_dir(&live).unwrap();
    fs::write(live.join("source.txt"), b"recoverable source bytes").unwrap();
    let release = f._dir.path().join("release-child");
    let child = Command::new("python3").args(["-c","import os,sys,time; sys.stdin.read();\nwhile not os.path.exists(sys.argv[1]): time.sleep(0.002)\n"])
        .arg(&release).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let resources = Resources {
        live,
        store: LocalStore::open(f._dir.path().join("cas")).unwrap(),
        checkpoint_calls: Arc::new(AtomicUsize::new(0)),
        retire_calls: Arc::new(AtomicUsize::new(0)),
        retired: Arc::new(AtomicBool::new(false)),
        fail_checkpoint: false,
        fail_retirement_once: false,
        cancel_after_retirement: false,
    };
    (
        LocalRuntimeDrain::new(
            f.owner.clone(),
            f.observation.clone(),
            child,
            workers,
            resources,
            original,
        ),
        release,
    )
}

struct HeldTls {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for HeldTls {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}
thread_local! { static HELD:RefCell<Option<HeldTls>>=const{RefCell::new(None)}; }

#[test]
fn ordinary_drain_retains_actual_child_and_tls_until_join_then_persists_pause_releases_and_retires()
{
    let mut f = fixture();
    let pending = f
        .journal
        .accept_command(&f.token, &request("pending", "target"), 2)
        .unwrap();
    let original = FinalizationBudget::begin(Duration::from_secs(10)).unwrap();
    let (mut drain, release_child) = owned_runtime(&f, vec![], original.clone());
    let pid = drain.original_process_id();
    let stdout = drain.child.stdout.take().unwrap();
    let (entered, waiting) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let worker = OwnedWorkerJoin::new(thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stdout = stdout;
        stdout.read_to_end(&mut bytes).unwrap();
        HELD.with(|slot| {
            *slot.borrow_mut() = Some(HeldTls {
                entered,
                release: released,
            })
        });
    }));
    drain.workers.push(worker.clone());
    assert_eq!(
        drain.advance_off_loop(&mut f.journal, 3).unwrap(),
        RuntimeDrainPhase::Checkpointed
    );
    assert_eq!(drain.original_process_id(), pid);
    assert_eq!(drain.original_budget().constraint(), original.constraint());
    assert!(drain.exit_status().is_none());
    assert!(f.journal.project_state("p").unwrap().owner_active);
    assert!(f
        .journal
        .accept_command(&f.token, &request("late", "target"), 3)
        .is_err());
    let rejected = f
        .journal
        .command_receipt(&f.owner, &pending.command_id)
        .unwrap();
    assert_eq!(rejected.result.unwrap()["status"], "rejected");
    // A surface disconnect does not replace the actual process/worker carrier.
    f.journal = Journal::open(&f.path).unwrap();
    fs::write(&release_child, b"release").unwrap();
    waiting.recv_timeout(Duration::from_secs(3)).unwrap();
    let wait_limit = Instant::now() + Duration::from_secs(3);
    while drain.phase() != RuntimeDrainPhase::ProcessJoined {
        assert!(Instant::now() < wait_limit);
        drain.advance_off_loop(&mut f.journal, 4).unwrap();
        thread::yield_now();
    }
    assert!(drain.exit_status().unwrap().success());
    assert!(
        worker.try_joined().unwrap().is_none(),
        "TLS destructor is still held"
    );
    assert!(f.journal.project_state("p").unwrap().owner_active);
    assert!(drain.resources.live.exists());
    assert_eq!(drain.resources.retire_calls.load(Ordering::SeqCst), 0);
    assert!(f
        .journal
        .events("p", 0, 1000)
        .unwrap()
        .iter()
        .all(|e| e.kind != "runtime-paused"));
    release.send(()).unwrap();
    drain.await_retired_off_loop(&mut f.journal, || 5).unwrap();
    assert_eq!(drain.phase(), RuntimeDrainPhase::Retired);
    assert!(worker.try_joined().unwrap().is_some());
    assert!(drain.resources.retired.load(Ordering::SeqCst));
    assert!(!f.journal.project_state("p").unwrap().owner_active);
    assert!(!drain.resources.live.exists());
    let checkpoint = drain.checkpoint_reference().unwrap();
    assert!(!drain
        .resources
        .store
        .read_object(checkpoint)
        .unwrap()
        .is_empty());
    let events = f.journal.events("p", 0, 1000).unwrap();
    let paused = events.iter().find(|e| e.kind == "runtime-paused").unwrap();
    assert_eq!(paused.payload["state"], "paused");
    assert!(paused.payload["process"].is_null());
    assert_eq!(paused.payload["original_process_id"], pid);
    let released = events.iter().find(|e| e.kind == "owner-released").unwrap();
    assert!(paused.sequence < released.sequence);
    assert!(f
        .journal
        .observe_runtime(&f.owner, &f.observation, 6)
        .is_err());
    for _ in 0..3 {
        assert_eq!(
            drain.advance_off_loop(&mut f.journal, 6).unwrap(),
            RuntimeDrainPhase::Retired
        );
    }
    assert_eq!(drain.resources.checkpoint_calls.load(Ordering::SeqCst), 1);
    assert_eq!(drain.resources.retire_calls.load(Ordering::SeqCst), 1);
    // The actual original child has been reaped, rather than only signalled.
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn drain_checkpoint_failure_closes_admission_and_retains_process_without_automatic_retry() {
    let mut f = fixture();
    let (mut drain, release) = owned_runtime(
        &f,
        vec![],
        FinalizationBudget::begin(Duration::from_secs(5)).unwrap(),
    );
    drain.resources.fail_checkpoint = true;
    assert!(drain.advance_off_loop(&mut f.journal, 2).is_err());
    assert!(
        drain.child.stdin.is_some(),
        "failed checkpoint must not send EOF"
    );
    assert!(drain.child.try_wait().unwrap().is_none());
    assert!(f.journal.project_state("p").unwrap().owner_active);
    assert!(f
        .journal
        .accept_command(&f.token, &request("late", "target"), 3)
        .is_err());
    assert!(drain.advance_off_loop(&mut f.journal, 3).is_err());
    assert_eq!(drain.resources.checkpoint_calls.load(Ordering::SeqCst), 1);
    assert_eq!(drain.resources.retire_calls.load(Ordering::SeqCst), 0);
    assert!(drain
        .reconcile_checkpoint_failure(
            &mut f.journal,
            &RuntimeCheckpointResolution::RetrySafe {
                evidence: serde_json::Value::Null
            },
            4
        )
        .is_err());
    drain
        .reconcile_checkpoint_failure(
            &mut f.journal,
            &RuntimeCheckpointResolution::RetrySafe {
                evidence: json!({"checkpoint_store":"original attempt confirmed absent"}),
            },
            4,
        )
        .unwrap();
    drain.resources.fail_checkpoint = false;
    fs::write(release, b"release").unwrap();
    drain.await_retired_off_loop(&mut f.journal, || 5).unwrap();
    assert_eq!(drain.phase(), RuntimeDrainPhase::Retired);
    assert_eq!(drain.resources.checkpoint_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn cancelled_original_deadline_cannot_be_replaced_by_repeated_polls_or_late_process_exit() {
    let mut f = fixture();
    let original = FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
    let (mut drain, release) = owned_runtime(&f, vec![], original.clone());
    drain.advance_off_loop(&mut f.journal, 2).unwrap();
    original.cancel();
    fs::write(release, b"release").unwrap();
    drain.child.wait().unwrap(); // Fixture observes a late physical exit.
    for _ in 0..3 {
        assert!(drain.advance_off_loop(&mut f.journal, 3).is_err());
    }
    assert_eq!(drain.original_budget().constraint(), original.constraint());
    assert!(f.journal.project_state("p").unwrap().owner_active);
    assert!(drain.resources.live.exists());
    assert_eq!(drain.resources.retire_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn delayed_uncertain_effect_blocks_release_after_actual_child_join_until_reconciliation() {
    let mut f = fixture();
    let accepted = f
        .journal
        .accept_command(&f.token, &request("started", "target"), 2)
        .unwrap();
    let claim = f
        .journal
        .claim_effect(&f.owner, &accepted.command_id, 2)
        .unwrap()
        .unwrap();
    f.journal
        .mark_effect_uncertain(&claim, &json!({"transport":"disconnected"}), 2)
        .unwrap();
    let (mut drain, release) = owned_runtime(
        &f,
        vec![],
        FinalizationBudget::begin(Duration::from_secs(5)).unwrap(),
    );
    fs::write(release, b"release").unwrap();
    assert!(drain.await_retired_off_loop(&mut f.journal, || 3).is_err());
    assert_eq!(drain.phase(), RuntimeDrainPhase::TerminalPersisted);
    assert!(drain.exit_status().is_some());
    assert!(f.journal.project_state("p").unwrap().owner_active);
    assert!(drain.resources.live.exists());
    assert!(f
        .journal
        .finish_effect(&claim, &json!({"late":true}), 4)
        .is_err());
    f.journal
        .reconcile_effect(
            &f.owner,
            &accepted.command_id,
            &Reconciliation::Completed {
                result: json!({"late":true}),
                evidence: json!({"external_receipt":"original exact command reconciled"}),
            },
            4,
        )
        .unwrap();
    drain.await_retired_off_loop(&mut f.journal, || 5).unwrap();
    assert_eq!(drain.phase(), RuntimeDrainPhase::Retired);
    assert_eq!(drain.resources.checkpoint_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.journal
            .events("p", 0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "runtime-paused")
            .count(),
        1
    );
}

#[test]
fn failed_resource_retirement_keeps_original_owner_and_checkpoint_without_releasing_twice() {
    let mut f = fixture();
    let (mut drain, release) = owned_runtime(
        &f,
        vec![],
        FinalizationBudget::begin(Duration::from_secs(5)).unwrap(),
    );
    drain.resources.fail_retirement_once = true;
    fs::write(release, b"release").unwrap();
    assert!(drain.await_retired_off_loop(&mut f.journal, || 3).is_err());
    assert_eq!(drain.phase(), RuntimeDrainPhase::OwnerReleased);
    assert!(!f.journal.project_state("p").unwrap().owner_active);
    assert!(drain.resources.live.exists());
    assert!(drain.checkpoint_reference().is_some());
    drain.await_retired_off_loop(&mut f.journal, || 4).unwrap();
    assert_eq!(drain.phase(), RuntimeDrainPhase::Retired);
    assert_eq!(
        f.journal
            .events("p", 0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "owner-released")
            .count(),
        1
    );
    assert_eq!(drain.resources.checkpoint_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn stale_run_epoch_refuses_drain_before_checkpoint_or_process_signal() {
    let mut f = fixture();
    let (mut drain, _release) = owned_runtime(
        &f,
        vec![],
        FinalizationBudget::begin(Duration::from_secs(5)).unwrap(),
    );
    f.journal
        .advance_run_epoch(&f.owner, "target", 1, 2)
        .unwrap();
    assert!(drain.advance_off_loop(&mut f.journal, 3).is_err());
    assert!(drain.child.stdin.is_some());
    assert_eq!(drain.resources.checkpoint_calls.load(Ordering::SeqCst), 0);
    assert!(drain.resources.live.exists());
    drain.child.kill().unwrap();
    drain.child.wait().unwrap(); // Fixture teardown only.
}

#[test]
fn two_original_runtimes_share_one_release_and_keep_each_actual_resource_owner() {
    let mut f = fixture();
    let mut second_fixture = fixture();
    second_fixture.observation.run_id = "other".into();
    second_fixture.observation.attempt_id = "other-attempt".into();
    f.journal
        .observe_runtime(&f.owner, &second_fixture.observation, 2)
        .unwrap();
    let (mut first, release_first) = owned_runtime(
        &f,
        vec![],
        FinalizationBudget::begin(Duration::from_secs(5)).unwrap(),
    );
    let (mut second, release_second) = owned_runtime(
        &second_fixture,
        vec![],
        FinalizationBudget::begin(Duration::from_secs(5)).unwrap(),
    );
    fs::write(release_first, b"release").unwrap();
    assert!(first.await_retired_off_loop(&mut f.journal, || 3).is_err());
    assert_eq!(first.phase(), RuntimeDrainPhase::TerminalPersisted);
    assert!(first.resources.live.exists());
    fs::write(release_second, b"release").unwrap();
    second.await_retired_off_loop(&mut f.journal, || 4).unwrap();
    first.await_retired_off_loop(&mut f.journal, || 5).unwrap();
    assert_eq!(first.phase(), RuntimeDrainPhase::Retired);
    assert_eq!(second.phase(), RuntimeDrainPhase::Retired);
    assert_eq!(first.resources.retire_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.resources.retire_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.journal
            .events("p", 0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "owner-released")
            .count(),
        1
    );
}

#[test]
fn retirement_finishing_after_the_original_deadline_is_retained_as_late_not_normal_success() {
    let mut f = fixture();
    let (mut drain, release) = owned_runtime(
        &f,
        vec![],
        FinalizationBudget::begin(Duration::from_secs(5)).unwrap(),
    );
    drain.resources.cancel_after_retirement = true;
    fs::write(release, b"release").unwrap();
    assert!(drain.await_retired_off_loop(&mut f.journal, || 3).is_err());
    assert_eq!(drain.phase(), RuntimeDrainPhase::OwnerReleased);
    assert!(
        drain.resources.retired.load(Ordering::SeqCst),
        "actual late result remains retained"
    );
    assert!(drain.advance_off_loop(&mut f.journal, 4).is_err());
    assert_eq!(drain.resources.retire_calls.load(Ordering::SeqCst), 1);
}
