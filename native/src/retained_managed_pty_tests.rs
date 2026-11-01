//! Actual PTYs/children/readers; fault seams choose errors, never synthesize exit.
use super::*;
use std::cell::{Cell, RefCell};
thread_local! {
    static FAULT: Cell<Option<(RetainingStep,bool)>> = const { Cell::new(None) };
    static NEXT_GATE: RefCell<Option<ReaderGate>> = const { RefCell::new(None) };
    static READER_GATE: RefCell<Option<ReaderGate>> = const { RefCell::new(None) };
}
pub(super) fn fault_step(step: RetainingStep) -> Result<()> {
    let fault=FAULT.with(|fault| { let value=fault.get();if value.is_some_and(|(at,_)|at==step) { fault.set(None);value } else { None } });
    match fault { Some((_,true))=>panic!("injected after real PTY ownership"),Some((_,false))=>Err(anyhow!("injected retained PTY failure")),None=>Ok(()) }
}
pub(super) struct ReaderGate { entered:SyncSender<()>, released:Receiver<()> }
impl Drop for ReaderGate { fn drop(&mut self) { let _=self.entered.send(());let _=self.released.recv_timeout(Duration::from_secs(5)); } }
pub(super) fn take_reader_gate()->Option<ReaderGate> { NEXT_GATE.with(|gate|gate.borrow_mut().take()) }
pub(super) fn install_reader_gate(gate:Option<ReaderGate>) { READER_GATE.with(|slot|*slot.borrow_mut()=gate); }
fn budget()->FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(3)).unwrap() }
fn blocked_command()->TerminalCommand { TerminalCommand::with_args("/bin/sh",["-c","exec /bin/sleep 30"]) }
fn verify_reaped(owner:&mut RetainedPtySpawn) {
    let child=owner.child_mut().expect("original real child retained");
    assert!(child.try_wait().unwrap().is_some());
    assert!(child.pending.is_none(),"retaining failure must not hand child to detached global reaper");
    assert!(child.reservation.is_none(),"actual wait releases the original admission");
}
#[test]
fn partial_spawn_failures_keep_the_original_live_child_until_explicit_reap() {
    for step in [RetainingStep::AfterChild,RetainingStep::AfterReader,RetainingStep::AfterWriter,
        RetainingStep::BeforeReaderSpawn,RetainingStep::AfterReaderSpawn] {
        let mut owner=RetainedPtySpawn::default();FAULT.with(|fault|fault.set(Some((step,false))));
        assert!(owner.spawn(Some(blocked_command()),TerminalPaneOptions::default()).is_err());
        let child=owner.child_mut().unwrap();let identity=child.identity.clone();
        assert!(child.try_wait().unwrap().is_none());assert!(child.pending.is_none());assert!(child.reservation.is_some());
        owner.settle_off_app(&budget()).unwrap();verify_reaped(&mut owner);
        assert!(Arc::ptr_eq(&identity,&owner.child_mut().unwrap().identity));
        if step==RetainingStep::AfterReaderSpawn { assert!(owner.reader_joined.is_some(),"actual spawned reader must join"); }
    }
}
#[test]
fn ordinary_native_unix_exec_error_is_known_absent_and_recovers() {
    let mut owner=RetainedPtySpawn::default();
    let missing=format!("/rudder-no-executable-{}",uuid::Uuid::new_v4());
    assert!(owner.spawn(Some(TerminalCommand::with_args(missing,Vec::<String>::new())),TerminalPaneOptions::default()).is_err());
    assert!(owner.child.is_none());assert!(!owner.spawn_in_flight);
    owner.settle_off_app(&budget()).unwrap();assert!(owner.settled);
    assert!(owner.reservation.is_none());assert!(owner.pair.is_none());
}
#[test]
fn caught_post_spawn_unwind_preserves_actual_child_custody() {
    let mut owner=RetainedPtySpawn::default();FAULT.with(|fault|fault.set(Some((RetainingStep::AfterReader,true))));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||owner.spawn(Some(blocked_command()),TerminalPaneOptions::default()))).is_err());
    assert!(owner.child.as_ref().unwrap().child.is_some());assert!(owner.reader.is_some());
    owner.settle_off_app(&budget()).unwrap();verify_reaped(&mut owner);
}
#[test]
fn reader_tls_timeout_retains_original_join_and_child_then_retry_actually_settles() {
    let (entered,saw_enter)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    NEXT_GATE.with(|gate|*gate.borrow_mut()=Some(ReaderGate { entered,released }));
    FAULT.with(|fault|fault.set(Some((RetainingStep::AfterReaderSpawn,false))));
    let mut owner=RetainedPtySpawn::default();
    assert!(owner.spawn(Some(blocked_command()),TerminalPaneOptions::default()).is_err());
    assert!(owner.settle_off_app(&FinalizationBudget::begin(Duration::from_millis(40)).unwrap()).is_err());
    saw_enter.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!owner.settled);assert!(owner.reader_join.is_some());assert!(owner.reader_joined.is_none());
    assert!(owner.child.as_ref().unwrap().child.is_some());
    release.send(()).unwrap();owner.settle_off_app(&budget()).unwrap();verify_reaped(&mut owner);
    assert!(owner.reader_joined.is_some());
}
#[test]
fn completed_retaining_spawn_transfers_the_same_original_child_to_the_pane() {
    let mut owner=RetainedPtySpawn::default();owner.spawn(Some(blocked_command()),TerminalPaneOptions::default()).unwrap();
    assert!(owner.child.is_none());let pane=owner.pane.as_ref().unwrap();
    assert!(pane.child.child.is_some());assert!(pane.reader_thread.is_some());
    let custody=pane.retain_managed_helper_custody().unwrap();
    assert!(pane.matches_managed_helper_custody(&custody));
    owner.settle_off_app(&budget()).unwrap();verify_reaped(&mut owner);assert!(owner.reader_joined.is_some());
}
#[test]
fn live_pane_cannot_enter_retired_background_capsule_and_refusal_keeps_its_original_reader() {
    let pane=TerminalPane::spawn_shell_or_command(Some(blocked_command()),TerminalPaneOptions::default()).unwrap();
    let custody=pane.retain_managed_helper_custody().unwrap();
    let mut pane=match ManagedPtyDrainOwner::retire(pane,&custody) { Err((pane,_))=>pane,Ok(_)=>panic!("live pane cannot be retired") };
    assert!(pane.reader_thread.is_some());assert!(pane.matches_managed_helper_custody(&custody));
    pane.request_termination();
    let until=Instant::now()+Duration::from_secs(3);
    while pane.try_wait().unwrap().is_none() { assert!(Instant::now()<until);thread::sleep(Duration::from_millis(5)); }
    let mut retired=match ManagedPtyDrainOwner::retire(pane,&custody) { Ok(retired)=>retired,Err(_)=>panic!("App actually observed original child reap") };
    retired.settle_off_app(&budget()).unwrap();assert!(retired.is_settled());assert!(retired.pty.reader_joined.is_some());
}
#[test]
fn checkpoint_match_uses_actual_successful_child_and_rejects_replacement_custody_and_wrong_data() {
    use rudder_engine::{execution::{ExecutionPhase,ExecutionRecord,ExecutionRecordV1},protocol::{ObjectKind,ObjectRef,OwnerEpoch},vfs::VfsCheckpoint};
    let mut original=RetainedPtySpawn::default();
    original.spawn(Some(TerminalCommand::with_args("/bin/sh",["-c","exit 0"])),TerminalPaneOptions::default()).unwrap();
    let pane=original.pane.as_mut().unwrap();let custody=pane.retain_managed_helper_custody().unwrap();
    let until=Instant::now()+Duration::from_secs(3);
    let exit=loop { pane.drain_output();if let Some(exit)=pane.successful_managed_helper_exit().unwrap() { break exit; }
        assert!(Instant::now()<until);thread::sleep(Duration::from_millis(5)); };
    // Public audit DATA only; never construct a fake ValidatedManagedShutdown.
    // The public matcher still requires the genuine private engine ACK owner.
    let record=ExecutionRecord::V1(ExecutionRecordV1 { format_version:1,execution_id:exit.instance_id.clone(),
        operation_id:uuid::Uuid::new_v4().to_string(),view_id:uuid::Uuid::new_v4().to_string(),
        owner:OwnerEpoch { owner:"fixture-owner".into(),epoch:1 },request_digest:"a".repeat(64),
        phase:ExecutionPhase::Checkpointed,supervisor_pid:exit.pid,agent_exit:Some(0),checkpoint:Some(VfsCheckpoint {
            format_version:1,directory:ObjectRef::from_bytes(ObjectKind::Directory,b"audit directory data"),
            hardlinks:ObjectRef::from_bytes(ObjectKind::Blob,b"audit topology data") }) });
    exit.matches_checkpoint_record(&custody,&record).unwrap();
    let mut replacement=RetainedPtySpawn::default();replacement.spawn(Some(blocked_command()),TerminalPaneOptions::default()).unwrap();
    let foreign=replacement.pane.as_ref().unwrap().retain_managed_helper_custody().unwrap();
    assert!(exit.matches_checkpoint_record(&foreign,&record).is_err());
    for change in 0..3 {
        let mut bad=record.clone();let ExecutionRecord::V1(data)=&mut bad else { unreachable!() };
        match change { 0=>data.execution_id=uuid::Uuid::new_v4().to_string(),1=>data.supervisor_pid+=1,_=>{data.phase=ExecutionPhase::Running;data.checkpoint=None;} }
        assert!(exit.matches_checkpoint_record(&custody,&bad).is_err());
    }
    original.settle_off_app(&budget()).unwrap();replacement.settle_off_app(&budget()).unwrap();
}
