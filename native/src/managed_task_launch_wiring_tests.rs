// Actual owner/writer/admission and actual worker/TLS custody tests. These do
// not construct a seed, helper receipt, model readiness or production startup.
use crate::{engine_host::{EngineOwnership,NativeAuthority},managed_task_launch::{ManagedTaskController,
    ManagedTaskOptIn,SelectedTaskCredentialKind,ManagedTaskNetworkSelection}};
use std::cell::RefCell;

struct Directory(PathBuf);
impl Directory { fn new()->Self {
    let base=if Path::new("/workspace").is_dir() { PathBuf::from("/workspace") } else { std::env::temp_dir() };
    let path=base.join(format!("rudder-task-wiring-{}",uuid::Uuid::new_v4()));
    fs::create_dir(&path).unwrap();Self(path.canonicalize().unwrap())
} }
impl Drop for Directory { fn drop(&mut self) { let _=fs::remove_dir_all(&self.0); } }
fn selected()->ManagedTaskOptIn { ManagedTaskOptIn::selected("fixture-api".into(),Backend::Codex,
    SelectedTaskCredentialKind::ApiKeyV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap() }
struct Fixture { app:App,controller:ManagedTaskController,supervisor:Option<Arc<ManagedTaskLifetimeSupervisor>>,
    _owner:EngineOwnership,_directory:Directory }
impl Fixture {
    fn new()->Self {
        let directory=Directory::new();let repo=directory.0.join("repo");let home=directory.0.join("home");
        fs::create_dir(&repo).unwrap();fs::create_dir(&home).unwrap();
        let mut owner=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();
        let fence=owner.journal_parts().1.clone();
        let supervisor=ManagedTaskLifetimeSupervisor::new_off_app(owner.source_lease(&fence).unwrap(),owner.source_journal_factory(&fence).unwrap()).unwrap();
        let mut app=App::new();app.cwd=repo;
        let mut run=crate::gitio::agent_from_run_record(&app.cwd,serde_json::json!({"id":"task-wiring-row","task":"explicit fixture task",
            "mode":"execute","status":"stopped","backend":"codex","model":"fixture-model"})).unwrap();
        run.mode=AgentMode::Execute;run.status=AgentStatus::Stopped;run.backend=Backend::Codex;run.model="fixture-model".into();
        app.agents.push(run);
        Self { app,controller:ManagedTaskController::new(),supervisor:Some(supervisor),_owner:owner,_directory:directory }
    }
    fn admission(&self)->ManagedTaskAdmission { self.controller.admit(&self.app,0,selected()).unwrap() }
    fn slot(&self,admission:ManagedTaskAdmission)->TaskHandle {
        // Exercise cleanup custody only with REAL admission and Engine lease.
        // No source/seed/launch authority is synthesized by this fixture.
        let slot=self.supervisor.as_ref().unwrap().slots[0].clone();
        assert!(!slot.occupied.swap(true,Ordering::AcqRel));
        let generation=slot.generation.fetch_add(1,Ordering::AcqRel)+1;
        let (budget,cancelled)=admission.wiring_test_parts();
        { let mut carrier=slot.carrier.lock().unwrap();carrier.budget=Some(budget.clone());
            carrier.cancelled=cancelled.clone();carrier.admission=Some(admission); }
        TaskHandle { slot,generation,budget,cancelled }
    }
    fn pump(&self,done:impl Fn()->bool) {
        let until=Instant::now()+Duration::from_secs(5);
        while !done()&&Instant::now()<until { self.supervisor.as_ref().unwrap().poll();thread::yield_now(); }
        assert!(done(),"actual task worker/resource settlement did not finish");
    }
    fn finish(mut self) {
        self.pump(||self.supervisor.as_ref().unwrap().shutdown_ready());
        if let Err((_owner,error))=ManagedTaskLifetimeSupervisor::stop_off_app(self.supervisor.take().unwrap()) { panic!("{error}"); }
    }
}

struct TlsGate { entered:SyncSender<()>,release:Receiver<()> }
impl Drop for TlsGate { fn drop(&mut self) { self.entered.send(()).unwrap();self.release.recv().unwrap(); } }
thread_local! { static TLS_GATE:RefCell<Option<TlsGate>>=const { RefCell::new(None) }; }

#[test]
fn original_admission_survives_callback_return_and_cancel_until_actual_tls_join_and_unused_cleanup() {
    let f=Fixture::new();let admission=f.admission();let cancel=admission.cancellation();
    let task=f.slot(admission);let second=f.admission();
    let queue=WorkerQueue::<bool>::new();
    let (entered,saw_enter)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    queue.spawn_retaining(queue.admit().unwrap(),task.worker().unwrap(),None,move |_| {
        TLS_GATE.with(|slot|*slot.borrow_mut()=Some(TlsGate { entered,release:released }));true
    },||false).unwrap();
    assert!(queue.receive.recv_timeout(Duration::from_secs(2)).unwrap());
    saw_enter.recv_timeout(Duration::from_secs(2)).unwrap(); // callback returned, TLS still LIVE
    assert_eq!(queue.active.load(Ordering::Acquire),1,"callback return cannot release shared queue admission before TLS join");
    let other_queue_permit=queue.admit().unwrap();
    assert!(queue.admit().is_err(),"two queue admissions must include the original still-running TLS worker");
    drop(other_queue_permit);
    cancel.cancel();task.unused();f.supervisor.as_ref().unwrap().poll();
    assert!(task.time().is_err(),"same original cancellation/budget must reach queued work");
    assert!(f.controller.admit(&f.app,0,selected()).is_err(),"a result/cancel cannot free unsettled original admission");
    assert!(!f.supervisor.as_ref().unwrap().shutdown_ready());
    release.send(()).unwrap();
    f.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);
    assert_eq!(queue.active.load(Ordering::Acquire),0,"actual joined worker releases its original queue permit");
    let third=f.admission();drop(third);drop(second);drop(task);drop(queue);f.finish();
}

#[test]
fn failed_typed_result_delivery_still_returns_original_worker_to_off_app_cleanup() {
    let f=Fixture::new();let task=f.slot(f.admission());let second=f.admission();
    let queue=WorkerQueue::<bool>::new();let (release,released)=mpsc::sync_channel(1);
    queue.spawn_retaining(queue.admit().unwrap(),task.worker().unwrap(),None,move |_| { released.recv().unwrap();true },||false).unwrap();
    drop(queue.receive); // No App event can return ownership in this case.
    assert!(f.controller.admit(&f.app,0,selected()).is_err());
    release.send(()).unwrap();
    f.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);
    assert_eq!(task.slot.handles.load(Ordering::Acquire),0,"the ORIGINAL actual JoinHandle must settle");
    let third=f.admission();drop(third);drop(second);drop(task);f.finish();
}

#[test]
fn retired_result_alias_prevents_storage_reuse_and_shutdown_until_off_app_finalization() {
    let f=Fixture::new();let task=f.slot(f.admission());let old=task.clone();task.unused();
    f.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);
    let same=task.slot.clone();drop(task);old.unused();
    assert!(same.occupied.load(Ordering::Acquire));
    assert!(!f.supervisor.as_ref().unwrap().shutdown_ready(),"stale App/result aliases still hold actual clock/custody owners");
    drop(old);drop(same);f.finish();
}

#[test]
fn supervisor_stop_refusal_returns_same_actual_engine_owner_instead_of_dropping_live_slots() {
    let mut f=Fixture::new();let task=f.slot(f.admission());
    let owner=f.supervisor.take().unwrap();let identity=Arc::as_ptr(&owner);
    match ManagedTaskLifetimeSupervisor::stop_off_app(owner) {
        Ok(())=>panic!("occupied task lifetime must refuse Engine release"),
        Err((owner,_))=>{ assert_eq!(identity,Arc::as_ptr(&owner));f.supervisor=Some(owner); }
    }
    task.unused();f.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);drop(task);f.finish();
}

#[test]
fn checkpoint_acceptance_hold_blocks_shutdown_even_after_returned_handle_is_dropped() {
    let f=Fixture::new();let task=f.slot(f.admission());task.unused();
    f.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);
    let slot=task.slot.clone();slot.acceptance_pending.store(true,Ordering::Release);
    drop(task);f.supervisor.as_ref().unwrap().poll();
    assert!(!f.supervisor.as_ref().unwrap().shutdown_ready());
    // This exercises only the shutdown hold, not a fabricated completion/capsule.
    // Even loss of an external handle must not act as acceptance/discard ACK.
    drop(slot);for _ in 0..3 { f.supervisor.as_ref().unwrap().poll();thread::yield_now(); }
    assert!(!f.supervisor.as_ref().unwrap().shutdown_ready());
    // No task/helper/source was launched in this fixture. Explicitly release its
    // synthetic hold so the genuine owner and cleanup pool can be joined.
    let slot=&f.supervisor.as_ref().unwrap().slots[0];slot.acceptance_pending.store(false,Ordering::Release);
    slot.work.store(true,Ordering::Release);f.finish();
}
