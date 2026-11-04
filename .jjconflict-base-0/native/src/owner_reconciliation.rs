//! Explicit off-App recovery under the SAME retained checkout lock. External
//! reconciliation is performed by the caller's original protocol owner, never
//! by interpreting a PID, EOF, lease, stop request or persisted PTY flag here.
use super::*;
use rudder_engine::execution::finalization_budget::FinalizationBudget;

/// Trusted adapter contract, not a deserializable proof or a callback supplied
/// by an agent request. Implementors must retain their actual original process,
/// transport, enrolled worker joins and resource/checkpoint owners on failure.
/// They may settle only effects with authoritative original protocol receipts,
/// and custody only with actual retirement or authoritative terminal evidence.
/// Missing ownership/protocol support must return an error or leave quarantine.
pub(crate) trait OriginalExternalWork {
    fn reconcile_original(
        &mut self, journal: &mut Journal, previous: &OwnerFence,
        recovery: &NativeRestartRecovery, original: &FinalizationBudget,
    ) -> Result<()>;
}

/// Retained beside the actual lock BEFORE the CAS. An error during the CAS is
/// ambiguous, so this same attempt cannot be retried from journal readback.
#[derive(Default)]
pub(crate) struct GuardedReplacement {
    target: Option<String>, started: bool, acknowledgment: Option<OwnerFence>,
}
impl GuardedReplacement {
    pub(super) fn after_failure(started:bool)->Self {Self{started,..Self::default()}}
    pub(crate) fn acknowledgment(&self) -> Option<&OwnerFence> {self.acknowledgment.as_ref()}
    pub(crate) fn started(&self) -> bool {self.started}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudder_engine::{execution::handoff::OwnedWorkerJoin,journal::{RuntimeObservation,RuntimePhase}};
    use std::{process::{Child,Command},time::Duration};

    struct Fixture {root:PathBuf,repo:PathBuf,home:PathBuf,previous:OwnerFence,observation:RuntimeObservation}
    impl Fixture {
        fn new()->Self {
            let root=PathBuf::from("/workspace/rudder-scratch/harden-11").join(uuid::Uuid::new_v4().to_string());
            let repo=root.join("repo");let home=root.join("home");fs::create_dir_all(&repo).unwrap();fs::create_dir(&home).unwrap();
            let mut engine=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();
            let previous=engine.fence.clone();engine.journal.observe_run_attempt(&previous,"run","original-child",1).unwrap();
            let observation=RuntimeObservation{run_id:"run".into(),run_epoch:1,attempt_id:"original-child".into(),backend:"codex".into(),execution_profile:"local-test".into(),
                thread_id:Some("original-thread".into()),active_turn_id:Some("original-turn".into()),last_interrupted_turn_id:None,phase:RuntimePhase::Working,capabilities:["inspect".into()].into(),pty_owned:true};
            engine.journal.observe_runtime(&previous,&observation,1).unwrap();drop(engine);
            Self{root,repo,home,previous,observation}
        }
        fn refusal(&self)->anyhow::Error {EngineOwnership::acquire_in_home(&self.repo,NativeAuthority::Fabric,&self.home).err().unwrap()}
    }
    impl Drop for Fixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}

    /// This test adapter has the actual spawned child and EVERY enrolled test
    /// worker. It changes observation only after reaping and actual TLS join.
    struct OriginalChild {child:Child,worker:OwnedWorkerJoin,observation:RuntimeObservation,calls:usize}
    impl OriginalChild {
        fn new(observation:RuntimeObservation)->Self {Self{child:Command::new("sh").args(["-c","exit 0"]).spawn().unwrap(),
            worker:OwnedWorkerJoin::new(std::thread::spawn(||{})),observation,calls:0}}
    }
    impl OriginalExternalWork for OriginalChild {
        fn reconcile_original(&mut self,journal:&mut Journal,owner:&OwnerFence,recovery:&NativeRestartRecovery,original:&FinalizationBudget)->Result<()> {
            self.calls+=1;assert!(!recovery.runtimes.is_empty());
            loop {original.remaining()?;if self.child.try_wait()?.is_some(){break;}std::thread::sleep(Duration::from_millis(1));}
            let _actual_join=self.worker.await_joined(original.remaining()?)?;
            self.observation.pty_owned=false;self.observation.phase=RuntimePhase::Closed;self.observation.active_turn_id=None;
            journal.observe_runtime(owner,&self.observation,epoch_ms())?;Ok(())
        }
    }
    impl Drop for OriginalChild {fn drop(&mut self){let _=self.child.wait();let _=self.worker.await_joined(Duration::from_secs(2));}}
    struct NoProgress;
    impl OriginalExternalWork for NoProgress {
        fn reconcile_original(&mut self,_:&mut Journal,_:&OwnerFence,_:&NativeRestartRecovery,_:&FinalizationBudget)->Result<()> {Ok(())}
    }

    #[test]
    fn retained_reconciliation_reaps_original_child_and_joins_workers_before_single_transfer() {
        let f=Fixture::new();let mut actual=OriginalChild::new(f.observation.clone());let failure=f.refusal();
        let retained=failure.downcast_ref::<RetainedRestartFailure>().unwrap();let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
        assert!(acquire_checkout_lock(&f.repo).is_err());
        let fence=retained.reconcile_and_replace_retaining("replacement",&mut actual,&original).unwrap();
        assert_eq!(fence.epoch,f.previous.epoch+1);assert_eq!(actual.calls,1);
        assert_eq!(retained.reconcile_and_replace_retaining("replacement",&mut actual,&original).unwrap(),fence);
        assert_eq!(actual.calls,1);assert!(acquire_checkout_lock(&f.repo).is_err());
        assert!(retained.reconcile_and_replace_retaining("other",&mut actual,&original).is_err());
        assert!(retained.reconcile(|_,_|Ok(())).is_err(),"old reconciliation callback cannot mutate successor");
    }
    #[test]
    fn retained_reconciliation_never_treats_successful_callback_as_physical_retirement() {
        let f=Fixture::new();let failure=f.refusal();let retained=failure.downcast_ref::<RetainedRestartFailure>().unwrap();
        let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
        assert!(retained.reconcile_and_replace_retaining("replacement",&mut NoProgress,&original).unwrap_err().to_string().contains("no authoritative progress"));
        retained.reconcile(|journal,owner|{assert_eq!(journal.project_state(&owner.project_id)?.owner,f.previous);Ok(())}).unwrap();
        assert!(acquire_checkout_lock(&f.repo).is_err());
    }
    #[test]
    fn unknown_transfer_cannot_be_replayed_or_settled_from_journal_flags() {
        let f=Fixture::new();let mut journal=Journal::open(&FabricPaths::new(&f.home).journal).unwrap();
        let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();let mut attempt=GuardedReplacement::after_failure(true);
        assert!(reconcile_locked_owner(&mut journal,&f.previous,"replacement",Some(&mut NoProgress),&original,&mut attempt,&mut None)
            .unwrap_err().to_string().contains("outcome is unknown"));
        assert_eq!(journal.project_state(&f.previous.project_id).unwrap().owner,f.previous);
    }
}

pub(crate) fn reconcile_locked_owner(
    journal: &mut Journal, previous: &OwnerFence, next_owner: &str,
    mut external: Option<&mut dyn OriginalExternalWork>, original: &FinalizationBudget,
    attempt: &mut GuardedReplacement, report: &mut Option<NativeRestartRecovery>,
) -> Result<OwnerFence> {
    original.remaining()?;
    if let Some(target) = &attempt.target {
        anyhow::ensure!(target == next_owner, "original replacement target changed");
    } else {attempt.target = Some(next_owner.to_owned());}
    if let Some(ack) = &attempt.acknowledgment {return Ok(ack.clone());}
    anyhow::ensure!(!attempt.started,
        "original transfer outcome is unknown; retain original lock and reconcile its transaction acknowledgment");
    // recover() returns bounded effect pages. Re-inspect after each actual
    // protocol settlement so a full page cannot hide additional uncertainty.
    // Runtime inventory itself fails closed above its fixed 4096-owner bound.
    let mut previous_page = None;
    for _ in 0..32 {
        original.remaining()?;
        match inspect_locked_owner(journal, previous, epoch_ms(), report) {
            Ok(()) => {
                original.remaining()?;
                attempt.started = true;
                let fence = journal.transfer_recovered_owner(previous, next_owner, epoch_ms())?;
                // Retain the ORIGINAL returned transaction ACK before any
                // fallible clock, filesystem or continuation checks.
                attempt.acknowledgment = Some(fence.clone());
                return Ok(fence);
            }
            Err(error) if error.downcast_ref::<RestartBlocked>().is_some() => {
                let recovery = report.as_ref().context("original recovery report absent")?;
                let page = serde_json::to_vec(recovery)?;
                anyhow::ensure!(previous_page.as_ref() != Some(&page),
                    "original external reconciliation made no authoritative progress; retain quarantine");
                previous_page = Some(page);
                let Some(actual) = external.as_deref_mut() else {return Err(error);};
                actual.reconcile_original(journal, previous, recovery, original)?;
                original.remaining()?;
                anyhow::ensure!(journal.project_state(&previous.project_id)?.owner == *previous,
                    "external reconciliation changed original authority");
            }
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!("original external reconciliation exceeds bounded pages; retain all owners")
}
