use std::cell::RefCell;

pub(crate) struct SpawnProbe {
    entered:mpsc::SyncSender<()>, release:mpsc::Receiver<()>,
    ended:Arc<AtomicBool>, expired:Arc<AtomicBool>,
}
struct TlsHold(SpawnProbe);
impl Drop for TlsHold {
    fn drop(&mut self) {
        let _ = self.0.entered.send(());
        if self.0.release.recv_timeout(Duration::from_secs(5)).is_err() {
            self.0.expired.store(true, Ordering::Release);
        }
        self.0.ended.store(true, Ordering::Release);
    }
}
thread_local! {
    static TLS:RefCell<Option<TlsHold>> = const { RefCell::new(None) };
    static PLAN:RefCell<Option<Plan>> = const { RefCell::new(None) };
}
struct Release(Option<mpsc::SyncSender<()>>);
impl Release { fn open(&mut self) { self.0.take().unwrap().send(()).unwrap(); } }
impl Drop for Release { fn drop(&mut self) { if let Some(send) = self.0.take() { let _ = send.try_send(()); } } }
struct PanicData { may_panic:Arc<AtomicBool>, dropped:Arc<AtomicBool> }
impl Drop for PanicData {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
        assert!(!self.may_panic.load(Ordering::Acquire), "raw panic payload must remain retained");
    }
}
struct Plan { name:&'static str, calls:usize, fail_on:usize, first:Option<SpawnProbe>, panic:Option<PanicData> }
struct Scope(Option<Plan>);
impl Scope {
    fn install(plan:Plan) -> Self { Self(PLAN.with(|slot|slot.replace(Some(plan)))) }
}
impl Drop for Scope { fn drop(&mut self) { PLAN.with(|slot| { slot.replace(self.0.take()); }); } }
// Caller-thread scoped, cfg(test) only. No environment/global runtime backdoor.
// The common Worker::spawn seam is also used by ResolverPool's real startup.
pub(crate) fn before_spawn(name:&str) -> Result<Option<SpawnProbe>> {
    PLAN.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(plan) = slot.as_mut().filter(|plan|plan.name == name) else { return Ok(None); };
        plan.calls += 1;
        if plan.calls == plan.fail_on {
            if let Some(payload) = plan.panic.take() { std::panic::panic_any(payload); }
            return Err(Error::new(Kind::ResourceLimit));
        }
        Ok(plan.first.take())
    })
}
pub(crate) fn install_probe(probe:Option<SpawnProbe>) {
    if let Some(probe) = probe { TLS.with(|slot|*slot.borrow_mut() = Some(TlsHold(probe))); }
}
fn probe() -> (SpawnProbe, Release, mpsc::Receiver<()>, Arc<AtomicBool>, Arc<AtomicBool>) {
    let (entered,wait) = mpsc::sync_channel(1); let (send,release) = mpsc::sync_channel(1);
    let ended = Arc::new(AtomicBool::new(false)); let expired = Arc::new(AtomicBool::new(false));
    (SpawnProbe { entered, release, ended:ended.clone(), expired:expired.clone() }, Release(Some(send)), wait, ended, expired)
}
fn owner(shared:Arc<Runtime>) -> OwnedConnectProxy {
    let (_send,ready) = mpsc::sync_channel(1);
    OwnedConnectProxy { shared, workers:Vec::with_capacity(8), ready:Some(ready), settled:false }
}

#[test]
fn body_returned_but_actual_tls_join_times_out_and_retries_same_observer() {
    let fixture = Fixture::new(); let (probe,mut release,entered,ended,expired) = probe();
    let worker = Worker::spawn("connect-lifecycle-tls", move ||install_probe(Some(probe))).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut owner = owner(fixture.shared.clone()); owner.workers.push(worker);
    let started = Instant::now();
    assert!(matches!(owner.close_and_settle(Duration::from_millis(20)), Err(error) if error.kind() == Kind::Unsettled));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(owner.workers[0].join.is_some() && !ended.load(Ordering::Acquire));
    assert!(fixture.shared.witnesses.lock().unwrap().is_some());
    let original = owner.workers[0].observer.as_ref().unwrap()._thread.thread().id();
    release.open(); let proof = owner.close_and_settle(Duration::from_secs(1)).unwrap();
    assert!(ended.load(Ordering::Acquire) && !expired.load(Ordering::Acquire));
    assert_eq!(proof._workers[0].observer.as_ref().unwrap()._thread.thread().id(), original);
    assert!(proof._workers[0].join.is_none());
    assert!(fixture.shared.witnesses.lock().unwrap().is_none());
}

#[test]
fn observer_spawn_failure_retains_the_original_actual_handle_and_retry() {
    let (probe,mut release,entered,ended,expired) = probe();
    let mut worker = Worker::spawn("connect-lifecycle-observer", move ||install_probe(Some(probe))).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let original = worker.join.as_ref().unwrap().lock().unwrap().worker.as_ref().unwrap().thread().id();
    let error = worker.launch_observer_with(|work| {
        drop(work); // The same closure-drop behavior as an actual failed spawn.
        Err(std::io::Error::from_raw_os_error(libc::EAGAIN))
    }).unwrap_err();
    assert_eq!(error.kind(), Kind::ResourceLimit); assert!(worker.observer.is_none());
    assert_eq!(worker.join.as_ref().unwrap().lock().unwrap().worker.as_ref().unwrap().thread().id(), original);
    assert!(!ended.load(Ordering::Acquire)); release.open();
    worker.settle(Instant::now() + Duration::from_secs(1)).unwrap();
    assert!(ended.load(Ordering::Acquire) && !expired.load(Ordering::Acquire));
}

#[test]
fn panicking_payload_is_retained_and_cannot_turn_a_retry_into_success() {
    let may_panic = Arc::new(AtomicBool::new(true)); let dropped = Arc::new(AtomicBool::new(false));
    let payload = PanicData { may_panic:may_panic.clone(), dropped:dropped.clone() };
    let mut worker = Worker::spawn("connect-lifecycle-panic", move ||std::panic::panic_any(payload)).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||worker.settle(Instant::now() + Duration::from_secs(1))));
    let retained = !dropped.load(Ordering::Acquire);
    may_panic.store(false, Ordering::Release); // Bounded safe test cleanup even on a negative control.
    assert!(matches!(result, Ok(Err(error)) if error.kind() == Kind::Unsettled));
    assert!(retained && worker.failed && worker.join.is_some());
    assert!(worker.settle(Instant::now() + Duration::from_secs(1)).is_err());
    assert!(!dropped.load(Ordering::Acquire)); drop(worker);
    let end = Instant::now() + Duration::from_secs(1);
    while !dropped.load(Ordering::Acquire) { assert!(Instant::now() < end); thread::yield_now(); }
}

fn two_dns(fixture:&mut Fixture) {
    use crate::execution::network::ExpectedNetworkBinding;
    // Revalidate exact configuration/digest; do not mutate a validated budget.
    let mut raw = fixture.shared.profile.profile.clone(); raw.budgets.max_dns_jobs = 2;
    let binding = raw.binding.clone(); let nonce = raw.readiness_nonce.clone();
    let expected = ExpectedNetworkBinding { project_id:&binding.project_id, owner:&binding.owner,
        owner_epoch:binding.owner_epoch.value(), run_id:&binding.run_id, run_epoch:binding.run_epoch.value(),
        request_id:&binding.request_id, execution_id:&binding.execution_id, operation_id:&binding.operation_id,
        view_id:&binding.view_id, network_id:&binding.network_id, readiness_nonce:&nonce };
    let host = HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST]).unwrap();
    let profile = raw.validate(&expected, Some(&host)).unwrap();
    let shared = Arc::get_mut(&mut fixture.shared).unwrap();
    shared.binding = ConnectBinding::for_profile(&profile).unwrap(); shared.profile = profile;
    fixture.shared.adopt_grant(fixture.shared.grant().unwrap()).unwrap();
}

#[test]
fn partial_resolver_spawn_error_or_panic_retains_actual_first_worker_until_join() {
    for panic in [false,true] {
        let mut fixture = Fixture::new(); two_dns(&mut fixture);
        let (probe,mut release,entered,ended,expired) = probe();
        let may_panic = Arc::new(AtomicBool::new(panic)); let dropped = Arc::new(AtomicBool::new(false));
        let payload = panic.then(||PanicData { may_panic:may_panic.clone(), dropped:dropped.clone() });
        let _scope = Scope::install(Plan { name:"rudder-connect-dns", calls:0, fail_on:2, first:Some(probe), panic:payload });
        let mut retained = Some(owner(fixture.shared.clone()));
        let shared = fixture.shared.clone();
        // Real ResolverPool startup plus a pinned no-network ELF. Its first
        // real helper is reaped by the real resolver worker before gated TLS.
        let program = ResolverProgram { executable:Arc::new(std::fs::File::open("/usr/bin/true").unwrap()) };
        let result = provision_retaining(retained.as_mut().unwrap(), |owner| {
            ResolverPool::start_retaining(&shared, program, &mut owner.workers).map(|_|())
        });
        let owner = retained.as_mut().unwrap();
        let raw_retained = !dropped.load(Ordering::Acquire);
        may_panic.store(false, Ordering::Release); // Prevent a test failure from panicking again during cleanup.
        assert_eq!(result.unwrap_err().kind(), if panic { Kind::Unsettled } else { Kind::ResourceLimit });
        assert!(Arc::ptr_eq(&owner.shared, &shared)); assert_eq!(owner.workers.len(), 1);
        assert!(owner.await_ready(Duration::from_millis(1)).is_err());
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(owner.close_and_settle(Duration::from_millis(20)).is_err());
        assert!(!ended.load(Ordering::Acquire) && owner.workers[0].join.is_some());
        assert!(shared.witnesses.lock().unwrap().is_some()); release.open();
        let settled = owner.close_and_settle(Duration::from_secs(1));
        assert!(ended.load(Ordering::Acquire) && !expired.load(Ordering::Acquire));
        if panic {
            assert!(raw_retained && settled.is_err() && !owner.settled);
            assert!(owner.workers[0].join.is_none()); // Actual thread joined; uncertainty still retained.
            assert!(shared.startup_panic.lock().unwrap().is_some());
            assert!(!dropped.load(Ordering::Acquire));
        } else { assert!(settled.is_ok()); }
    }
}

#[test]
fn one_panicked_worker_cannot_starve_cleanup_of_another_actual_worker() {
    let fixture = Fixture::new(); let (probe,mut release,entered,ended,expired) = probe();
    let mut owner = owner(fixture.shared.clone());
    let mut first = Worker::spawn("connect-lifecycle-first-panic", ||panic!("first worker failed")).unwrap();
    assert!(first.settle(Instant::now() + Duration::from_secs(1)).is_err());
    assert!(first.failed); owner.workers.push(first);
    owner.workers.push(Worker::spawn("connect-lifecycle-second-tls", move ||install_probe(Some(probe))).unwrap());
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(owner.close_and_settle(Duration::from_millis(20)).is_err());
    assert!(owner.workers[0].failed && owner.workers[1].observer.is_some());
    release.open(); assert!(owner.close_and_settle(Duration::from_secs(1)).is_err());
    assert!(ended.load(Ordering::Acquire) && !expired.load(Ordering::Acquire));
    assert!(owner.workers[1].join.is_none()); assert!(!owner.settled);
}
