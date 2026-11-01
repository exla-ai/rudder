// Actual Engine ownership, task admission and callback joins. These two tests
// isolate admission state only: they never spawn a helper or mint exit custody.
use crate::{engine_host::{EngineOwnership,NativeAuthority},managed_task_launch::{ManagedTaskController,
    ManagedTaskOptIn,SelectedTaskCredentialKind,ManagedTaskNetworkSelection}};
fn selected()->ManagedTaskOptIn { ManagedTaskOptIn::selected("fixture-api".into(),Backend::Codex,
    SelectedTaskCredentialKind::ApiKeyV1,ManagedTaskNetworkSelection::OrdinaryNetworkV1).unwrap() }
struct Fixture { app:App,controller:ManagedTaskController,supervisor:Option<Arc<ManagedTaskLifetimeSupervisor>>,
    _owner:EngineOwnership,path:PathBuf }
impl Fixture {
    fn new()->Self {
        let base=if Path::new("/workspace").is_dir() { PathBuf::from("/workspace") } else { std::env::temp_dir() };
        let path=base.join(format!("rudder-task-activation-admission-{}",uuid::Uuid::new_v4()));
        let repo=path.join("repo");let home=path.join("home");fs::create_dir_all(&repo).unwrap();fs::create_dir(&home).unwrap();
        let mut owner=EngineOwnership::acquire_in_home(&repo,NativeAuthority::Fabric,&home).unwrap();
        let fence=owner.journal_parts().1.clone();
        let supervisor=ManagedTaskLifetimeSupervisor::new_off_app(owner.source_lease(&fence).unwrap(),owner.source_journal_factory(&fence).unwrap()).unwrap();
        let mut app=App::new();app.cwd=repo;
        let mut row=crate::gitio::agent_from_run_record(&app.cwd,serde_json::json!({"id":"activation-admission-row","task":"fixture task",
            "mode":"execute","status":"stopped","backend":"codex","model":"fixture-model"})).unwrap();
        row.mode=AgentMode::Execute;row.status=AgentStatus::Stopped;row.backend=Backend::Codex;row.model="fixture-model".into();app.agents.push(row);
        Self { app,controller:ManagedTaskController::new(),supervisor:Some(supervisor),_owner:owner,path }
    }
    fn admission(&self)->ManagedTaskAdmission { self.controller.admit(&self.app,0,selected()).unwrap() }
    fn slot(&self)->TaskHandle {
        let admission=self.admission();let (budget,cancelled)=admission.wiring_test_parts();
        let slot=self.supervisor.as_ref().unwrap().slots[0].clone();assert!(!slot.occupied.swap(true,Ordering::AcqRel));
        let generation=slot.generation.fetch_add(1,Ordering::AcqRel)+1;
        { let mut carrier=slot.carrier.lock().unwrap();carrier.budget=Some(budget.clone());carrier.cancelled=cancelled.clone();carrier.admission=Some(admission); }
        TaskHandle { slot,generation,budget,cancelled }
    }
    fn pump(&self,done:impl Fn()->bool) {
        let until=Instant::now()+Duration::from_secs(5);
        while !done() && Instant::now()<until { self.supervisor.as_ref().unwrap().poll();thread::yield_now(); }
        assert!(done(),"actual callback/supervisor settlement deadline");
    }
    fn finish_without_helper(mut self,task:TaskHandle) {
        // Reset ONLY this test's admission simulation. No resource or helper was
        // created: assert that fact before clearing its simulated admission bit.
        { let carrier=task.slot.carrier.lock().unwrap();assert!(carrier.activation_failure.is_none()&&carrier.drain.is_none()
            &&carrier.ready_owner.is_none()&&carrier.request.is_none()&&carrier.packet.is_none()); }
        task.slot.may_have_helper.store(false,Ordering::Release);task.unused();
        self.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);drop(task);
        self.pump(||self.supervisor.as_ref().unwrap().shutdown_ready());
        if let Err((_owner,error))=ManagedTaskLifetimeSupervisor::stop_off_app(self.supervisor.take().unwrap()) { panic!("{error}"); }
        drop(self._owner);fs::remove_dir_all(&self.path).unwrap();
    }
}
fn finish_callback(f:&Fixture,task:&TaskHandle) {
    let queue=WorkerQueue::new();queue.spawn_retaining(queue.admit().unwrap(),task.worker().unwrap(),None,|_|true,||false).unwrap();
    assert!(queue.receive.recv_timeout(Duration::from_secs(2)).unwrap());
    f.pump(||task.slot.carrier.lock().unwrap().preparation_joined);
}
#[test]
fn in_flight_activation_keeps_preparation_charge_after_actual_callback_join() {
    let f=Fixture::new();let task=f.slot();let second=f.admission();
    task.adopted();finish_callback(&f,&task);
    assert!(task.slot.carrier.lock().unwrap().admission.is_some());
    assert!(f.controller.admit(&f.app,0,selected()).is_err(),"starting activation is not settled successful adoption");
    drop(second);f.finish_without_helper(task);
}
#[test]
fn successful_adoption_releases_preparation_charge_while_lifetime_storage_stays_owned() {
    let f=Fixture::new();let task=f.slot();let second=f.admission();
    task.adopted();finish_callback(&f,&task);task.activated();
    f.pump(||task.slot.carrier.lock().unwrap().admission.is_none());
    let third=f.admission();assert!(task.slot.occupied.load(Ordering::Acquire));
    assert!(!f.supervisor.as_ref().unwrap().shutdown_ready(),"prep release must not release actual lifetime storage");
    drop(third);drop(second);f.finish_without_helper(task);
}
