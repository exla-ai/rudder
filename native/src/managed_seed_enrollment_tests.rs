//! Actual Engine lock/lease, Journal/run epochs and immutable managed records.
//! No Root client/seed/helper/ACK is fabricated. Positive cross-process Root
//! enrollment and the App/helper barrier remain a separate physical qualifier.
use super::*;
use crate::{engine_host::{EngineOwnership, NativeAuthority}, managed_launch::ManagedRunSource,
    managed_store};
use rudder_engine::{journal::ManagedProjectPrepare, protocol::{ObjectKind, ObjectRef}};
use std::{fs, os::unix::fs::DirBuilderExt, path::{Path, PathBuf}, time::Duration};

struct Fixture {
    owner:EngineOwnership,
    lease:crate::engine_host::EngineSourceLease,
    journal:Journal,
    location:ManagedRunLocation,
    assignment:WorkspaceAssignment,
    directory:PathBuf,
}
impl Fixture {
    fn new()->Self {
        let base=if Path::new("/workspace").is_dir() { PathBuf::from("/workspace") } else { std::env::temp_dir() };
        let directory=base.join(format!("rudder-seed-enrollment-binding-{}",uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let repo=directory.join("repo");let home=directory.join("home");
        fs::DirBuilder::new().mode(0o700).create(&repo).unwrap();fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let mut owner=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();
        let (journal,fence)=owner.journal_parts();let fence=fence.clone();
        let request=ManagedProjectPrepare { request_id:uuid::Uuid::new_v4().to_string(),
            namespace:managed_store::namespace(&home,&repo,&fence.project_id,journal).unwrap() };
        let store=owner.prepare_managed_project(&request).unwrap();
        let lease=owner.source_lease(&fence).unwrap();let mut journal=lease.open_source_writer().unwrap();
        let attempt_id=uuid::Uuid::new_v4().to_string();let run_id="actual-enrollment-row";
        let run_epoch=journal.observe_run_attempt(&fence,run_id,&attempt_id,2).unwrap();
        let source=ManagedRunSource::Reserved { fence:ManagedLaunchFence { owner:fence,run_id:run_id.into(),run_epoch,
            attempt_id,request_id:uuid::Uuid::new_v4().to_string() },runnable_root:ObjectRef::from_bytes(ObjectKind::RunnableRoot,b"binding audit data") };
        let assignment=managed_store::tests::assignment(&source);let location=store.run(run_id).unwrap();
        location.persist_reservation(&source).unwrap();location.persist_assignment(&assignment).unwrap();
        Self { owner,lease,journal,location,assignment,directory }
    }
    fn check(&self,assignment:&WorkspaceAssignment,location:&ManagedRunLocation,seed:&ObjectRef,
        budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.lease.check_journal(&self.journal)?;
        current_assignment(&self.journal,self.lease.owner(),location,assignment,&assignment.fence,seed,budget,cancelled)?;
        self.lease.check_journal(&self.journal)
    }
    fn finish(self) {
        let Self {owner,lease,journal,location,assignment,directory}=self;
        drop(assignment);drop(location);drop(journal);drop(lease);drop(owner);fs::remove_dir_all(directory).unwrap();
    }
}
fn budget()->FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(5)).unwrap() }
#[test]
fn current_original_engine_run_and_immutable_assignment_pass_binding_checks_without_minting_ack() {
    let fixture=Fixture::new();let original=budget();let constraint=original.constraint();
    fixture.check(&fixture.assignment,&fixture.location,&fixture.assignment.seed,&original,&AtomicBool::new(false)).unwrap();
    assert_eq!(constraint,original.constraint(),"binding reads must not replace the original clock");fixture.finish();
}
#[test]
fn replacing_actual_journal_attempt_rejects_the_old_unchanged_assignment() {
    let mut fixture=Fixture::new();let original=budget();
    fixture.check(&fixture.assignment,&fixture.location,&fixture.assignment.seed,&original,&AtomicBool::new(false)).unwrap();
    let fence=&fixture.assignment.fence;
    fixture.journal.observe_run_attempt(&fence.owner,&fence.run_id,&uuid::Uuid::new_v4().to_string(),3).unwrap();
    assert!(fixture.check(&fixture.assignment,&fixture.location,&fixture.assignment.seed,&original,&AtomicBool::new(false)).is_err());
    assert_eq!(fixture.location.assignment(&fence.attempt_id).unwrap().as_ref(),Some(&fixture.assignment),"saved DATA did not become current after replacement");fixture.finish();
}
#[test]
fn changed_source_same_fence_and_seed_is_rejected_by_fresh_original_assignment_read() {
    let fixture=Fixture::new();let mut changed=fixture.assignment.clone();
    changed.source=ObjectRef::from_bytes(ObjectKind::SourceRoot,b"different source audit data");
    assert_eq!(changed.fence,fixture.assignment.fence);assert_eq!(changed.seed,fixture.assignment.seed);
    let error=fixture.check(&changed,&fixture.location,&fixture.assignment.seed,&budget(),&AtomicBool::new(false)).unwrap_err();
    assert!(error.to_string().contains("assignment changed"));fixture.finish();
}
#[test]
fn another_seed_or_run_cannot_use_the_original_owner_binding() {
    let fixture=Fixture::new();let other=ObjectRef::from_bytes(ObjectKind::Directory,b"different seed audit data");
    assert!(fixture.check(&fixture.assignment,&fixture.location,&other,&budget(),&AtomicBool::new(false)).is_err());
    let other_run=fixture.location.store().run("other-row").unwrap();
    assert!(fixture.check(&fixture.assignment,&other_run,&fixture.assignment.seed,&budget(),&AtomicBool::new(false)).is_err());fixture.finish();
}
#[test]
fn actual_original_cancel_and_expiry_refuse_binding_before_root_enrollment_without_clock_renewal() {
    let fixture=Fixture::new();let cancelled=AtomicBool::new(true);let original=budget();let constraint=original.constraint();
    let error=fixture.check(&fixture.assignment,&fixture.location,&fixture.assignment.seed,&original,&cancelled).unwrap_err();
    assert!(error.to_string().contains("cancelled"));assert_eq!(constraint,original.constraint());
    let expired=FinalizationBudget::begin(Duration::from_millis(10)).unwrap();let prior=expired.constraint();
    std::thread::sleep(Duration::from_millis(20));
    assert!(fixture.check(&fixture.assignment,&fixture.location,&fixture.assignment.seed,&expired,&AtomicBool::new(false)).is_err());
    assert_eq!(prior,expired.constraint());
    assert_eq!(fixture.journal.current_run_attempt(&fixture.assignment.fence.owner,&fixture.assignment.fence.run_id,fixture.assignment.fence.run_epoch).unwrap(),fixture.assignment.fence.attempt_id);
    fixture.finish();
}
