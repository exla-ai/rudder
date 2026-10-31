// Child of linux::supervision. These are actual supervisor/thread custody
// tests for unused actors; no helper, grant or checkpoint authority is made.
use super::*;
use std::{cell::RefCell, sync::Condvar};

const WAIT: Duration = Duration::from_secs(3);

fn fence(run: &str) -> ManagedLaunchFence {
    ManagedLaunchFence { owner: OwnerFence { project_id:"supervisor-test".into(), owner_id:"owner".into(), epoch:1 },
        run_id:run.into(), run_epoch:1, attempt_id:uuid::Uuid::new_v4().to_string(),
        request_id:uuid::Uuid::new_v4().to_string() }
}
fn unused() -> SourceOwnerExit { SourceOwnerExit::Settled { unused:true, retired:None } }
fn is_unused(result: &SourceOwnerJoined) -> bool {
    matches!(&result.result, Ok(SourceOwnerExit::Settled { unused:true, retired:None }))
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < end, "source supervisor did not reach expected state");
        thread::sleep(Duration::from_millis(1));
    }
}

#[derive(Clone)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);
impl Gate {
    fn new() -> Self { Self(Arc::new((Mutex::new(false),Condvar::new()))) }
    fn open(&self) { *held(&self.0.0) = true; self.0.1.notify_all(); }
    fn wait(&self) -> bool {
        let (open, _) = self.0.1.wait_timeout_while(held(&self.0.0), Duration::from_secs(15), |open|! *open)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *open
    }
}
struct TlsHold { gate:Gate, entered:mpsc::Sender<()>, ended:Arc<AtomicBool>, expired:Arc<AtomicBool> }
impl Drop for TlsHold {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        // A failed test cannot strand a TLS destructor indefinitely. Expiry is
        // recorded and asserted, never accepted as the intended release proof.
        if !self.gate.wait() { self.expired.store(true, Ordering::Release); }
        self.ended.store(true, Ordering::Release);
    }
}
thread_local! { static TLS: RefCell<Option<TlsHold>> = const { RefCell::new(None) }; }
struct TlsProbe { gate:Gate, ended:Arc<AtomicBool>, expired:Arc<AtomicBool> }
impl TlsProbe {
    fn release(&self) { self.gate.open(); }
    fn assert_finished(&self) {
        assert!(self.ended.load(Ordering::Acquire));
        assert!(!self.expired.load(Ordering::Acquire), "TLS fixture auto-release deadline expired");
    }
}
impl Drop for TlsProbe { fn drop(&mut self) { self.release(); } }

struct Case {
    supervisor:Arc<SourceOwnerLifetimeSupervisor>,
    reservations:Vec<Option<SourceSupervision>>,
    gates:Vec<Gate>,
    closed:bool,
}
impl Case {
    fn new() -> Self {
        Self { supervisor:SourceOwnerLifetimeSupervisor::new_off_app().unwrap(), reservations:Vec::new(), gates:Vec::new(), closed:false }
    }
    fn reserve(&mut self, run: &str) -> SourceSupervisionKey {
        let fence = fence(run); let mut reservation = None;
        // Release publishes the free slot before decrementing admission. A
        // transient conservative refusal is allowed during that handoff.
        wait_until(|| match self.supervisor.reserve(&fence) {
            Ok(owned) => { reservation = Some(owned); true }, Err(_) => false,
        });
        let reservation = reservation.unwrap();
        let key = reservation.key(); self.reservations.push(Some(reservation)); key
    }
    fn take_reservation(&mut self, key: &SourceSupervisionKey) -> SourceSupervision {
        self.reservations.iter_mut().find(|slot|slot.as_ref().is_some_and(|r|r.matches(key)))
            .expect("test owns the unique reservation").take().unwrap()
    }
    fn job(&self, key: &SourceSupervisionKey) -> SourceOwnerJoinHandoff {
        SourceOwnerJoinHandoff { fence:key.fence.clone(), supervision:key.clone(), worker:thread::spawn(unused) }
    }
    fn tls_job(&mut self, key: &SourceSupervisionKey) -> (SourceOwnerJoinHandoff, TlsProbe) {
        let gate = Gate::new(); self.gates.push(gate.clone());
        let ended = Arc::new(AtomicBool::new(false)); let expired = Arc::new(AtomicBool::new(false));
        let (entered, wait) = mpsc::channel();
        let hold = TlsHold { gate:gate.clone(), entered, ended:ended.clone(), expired:expired.clone() };
        let worker = thread::spawn(move || { TLS.with(|tls|*tls.borrow_mut() = Some(hold)); unused() });
        wait.recv_timeout(WAIT).expect("source actor body returned and entered TLS teardown");
        (SourceOwnerJoinHandoff { fence:key.fence.clone(), supervision:key.clone(), worker }, TlsProbe { gate, ended, expired })
    }
    fn submit(&self, job: SourceOwnerJoinHandoff) {
        if let Err(job) = self.supervisor.try_join(job) {
            for gate in &self.gates { gate.open(); }
            let _actual_join = job.join_off_app();
            panic!("valid source join admission was refused");
        }
    }
    fn joined(&self) -> SourceOwnerJoined {
        let mut result = None;
        wait_until(|| { result = self.supervisor.try_take_joined(); result.is_some() });
        result.unwrap()
    }
    fn adopt_and_release(&mut self, joined: SourceOwnerJoined) {
        if !is_unused(&joined) {
            self.supervisor.retain_unadopted(joined);
            panic!("unused test actor returned unresolved ownership");
        }
        let key = joined.supervision.clone(); drop(joined); // Test App adopts this actual joined unused result.
        self.take_reservation(&key).release_settled(None);
        wait_until(|| !self.supervisor.status()[key.slot].occupied);
    }
    fn finish(&mut self) {
        for gate in &self.gates { gate.open(); }
        let end = Instant::now() + WAIT;
        loop {
            // On assertion unwind, preserve the same production adoption and
            // release guards; never force FREE/stop or invent settlement.
            while let Some(joined) = self.supervisor.try_take_joined() {
                if is_unused(&joined) { drop(joined); }
                else { self.supervisor.retain_unadopted(joined); break; }
            }
            for reservation in &mut self.reservations {
                let Some(owned) = reservation.as_ref() else { continue };
                let key = owned.key(); let slot = &self.supervisor.shared.slots[key.slot];
                if slot.phase.load(Ordering::Acquire) == RESERVED {
                    reservation.take().unwrap().cancel_before_effects();
                } else if let Some(stored) = available(&slot.stored) {
                    if stored.settled && stored.result_taken && stored.result.is_none() && !stored.busy {
                        drop(stored); reservation.take().unwrap().release_settled(None);
                    }
                }
            }
            // Shutdown consumes the unique reservation into its dedicated
            // inbox. Only actual adopted results can pass this release API.
            for key in self.supervisor.keys().into_iter().flatten() {
                if let Some(stored) = available(&self.supervisor.shared.slots[key.slot].stored) {
                    if stored.shutdown && stored.settled && stored.result_taken && stored.result.is_none() && !stored.busy {
                        drop(stored); let _ = self.supervisor.request_release_shutdown_settled(&key);
                    }
                }
            }
            if self.supervisor.status().iter().all(|s|!s.occupied) { break; }
            if Instant::now() >= end { return; } // No false stopped proof on failed cleanup.
            thread::sleep(Duration::from_millis(1));
        }
        if let Ok(proof) = self.supervisor.clone().try_close_off_app() {
            assert!(!proof.join_worker_panicked); self.closed = true;
        }
    }
}
impl Drop for Case {
    fn drop(&mut self) { if !self.closed { self.finish(); } }
}

#[test]
fn supervisor_enforces_two_actual_slots_and_generation_reuse() {
    let mut case = Case::new(); let first = case.reserve("first"); let second = case.reserve("second");
    assert_ne!(first.slot, second.slot);
    assert!(case.supervisor.reserve(&fence("third")).is_err());
    assert_eq!(case.supervisor.shared.admission.load(Ordering::Acquire), LIMIT);
    case.take_reservation(&first).cancel_before_effects();
    wait_until(|| !case.supervisor.status()[first.slot].occupied);
    let replacement = case.reserve("replacement");
    assert_eq!(replacement.slot, first.slot); assert!(replacement.generation > first.generation);
    assert!(!case.supervisor.current(&first)); assert!(case.supervisor.current(&replacement));
    assert!(case.supervisor.reserve(&fence("fourth")).is_err());
    case.finish(); assert!(case.closed);
}

#[test]
fn supervisor_tls_join_and_result_adoption_both_precede_slot_release() {
    let mut case = Case::new(); let key = case.reserve("tls"); let _second = case.reserve("reserved");
    let (job, probe) = case.tls_job(&key); case.submit(job);
    wait_until(|| case.supervisor.status()[key.slot].busy);
    assert!(case.supervisor.try_take_joined().is_none());
    assert!(!probe.ended.load(Ordering::Acquire));
    assert!(case.supervisor.reserve(&fence("before-join")).is_err());
    probe.release(); let joined = case.joined(); probe.assert_finished(); assert!(is_unused(&joined));
    assert!(case.supervisor.status()[key.slot].occupied);
    assert_eq!(case.supervisor.shared.slots[key.slot].phase.load(Ordering::Acquire), APP_OWNS);
    assert!(case.supervisor.reserve(&fence("before-release")).is_err());
    case.supervisor.retain_unadopted(joined);
    wait_until(|| case.supervisor.status()[key.slot].result_ready);
    assert!(case.supervisor.reserve(&fence("returned-result")).is_err());
    let joined = case.joined(); case.adopt_and_release(joined);
    case.reserve("after-adoption"); case.finish(); assert!(case.closed);
}

#[test]
fn supervisor_busy_refusal_returns_the_original_actual_join_handle() {
    let mut case = Case::new(); let key = case.reserve("busy");
    let (first, probe) = case.tls_job(&key); case.submit(first);
    wait_until(|| case.supervisor.status()[key.slot].busy);
    let duplicate = case.job(&key); let identity = duplicate.worker.thread().id();
    let refused = match case.supervisor.try_join(duplicate) { Err(job) => job, Ok(()) => panic!("busy slot accepted another source handle") };
    assert_eq!(refused.worker.thread().id(), identity);
    assert!(same(&refused.supervision, &key));
    assert!(is_unused(&refused.join_off_app()));
    assert!(case.supervisor.status()[key.slot].occupied);
    assert!(case.supervisor.try_take_joined().is_none());
    probe.release(); let joined = case.joined(); case.adopt_and_release(joined); probe.assert_finished();
    case.finish(); assert!(case.closed);
}

#[test]
fn supervisor_foreign_and_stale_keys_refuse_without_losing_actual_handle() {
    let mut first = Case::new(); let mut other = Case::new(); let key = first.reserve("foreign");
    let job = first.job(&key); let identity = job.worker.thread().id();
    let refused = match other.supervisor.try_join(job) { Err(job) => job, Ok(()) => panic!("foreign supervisor accepted source custody") };
    assert_eq!(refused.worker.thread().id(), identity); assert!(other.supervisor.status().iter().all(|s|!s.occupied));
    first.submit(refused); let joined = first.joined(); first.adopt_and_release(joined);
    let current = first.reserve("current"); assert_eq!(current.slot, key.slot);
    let stale = first.job(&key); let identity = stale.worker.thread().id();
    let refused = match first.supervisor.try_join(stale) { Err(job) => job, Ok(()) => panic!("stale generation accepted source custody") };
    assert_eq!(refused.worker.thread().id(), identity); assert!(is_unused(&refused.join_off_app()));
    let mut mismatched = first.job(&current); mismatched.fence.run_epoch += 1;
    let identity = mismatched.worker.thread().id();
    let refused = match first.supervisor.try_join(mismatched) { Err(job) => job, Ok(()) => panic!("mismatched run fence accepted source custody") };
    assert_eq!(refused.worker.thread().id(), identity); assert!(is_unused(&refused.join_off_app()));
    assert!(first.supervisor.current(&current)); assert!(first.supervisor.try_take_joined().is_none());
    first.finish(); other.finish(); assert!(first.closed && other.closed);
}

#[test]
fn supervisor_shutdown_closes_admission_but_cannot_release_unjoined_or_unadopted_result() {
    let mut case = Case::new(); let key = case.reserve("shutdown");
    let (job, probe) = case.tls_job(&key);
    let mut slots = std::array::from_fn(|_|None);
    slots[0] = Some(SourceSlotShutdown { supervision:case.take_reservation(&key), worker:Some(job.worker),
        recovery:None, checkpoint:None, live:None });
    case.supervisor.retain_shutdown(SourceOwnerShutdownHandoff { slots });
    wait_until(|| { let s = case.supervisor.status()[key.slot]; s.shutdown && s.busy });
    assert!(case.supervisor.reserve(&fence("during-shutdown")).is_err());
    let retained = match case.supervisor.clone().try_close_off_app() { Err(actual) => actual, Ok(_) => panic!("TLS-blocked supervisor reported stopped") };
    assert!(Arc::ptr_eq(&retained, &case.supervisor)); drop(retained);
    case.supervisor.request_release_shutdown_settled(&key).unwrap();
    assert!(case.supervisor.status()[key.slot].occupied); assert!(case.supervisor.try_take_joined().is_none());
    probe.release();
    wait_until(|| { let s = case.supervisor.status()[key.slot]; s.result_ready && s.fault.is_some() });
    assert!(case.supervisor.status()[key.slot].occupied);
    assert!(case.supervisor.clone().try_close_off_app().is_err());
    let joined = case.joined(); assert!(is_unused(&joined)); probe.assert_finished(); drop(joined);
    case.supervisor.request_release_shutdown_settled(&key).unwrap();
    wait_until(|| !case.supervisor.status()[key.slot].occupied);
    assert!(case.supervisor.reserve(&fence("after-shutdown-release")).is_err());
    case.finish(); assert!(case.closed);
}

#[test]
fn supervisor_close_racing_reserve_never_stops_an_admitted_source() {
    let mut case = Case::new(); let start = Gate::new(); case.gates.push(start.clone());
    let reserving = case.supervisor.clone(); let reserve_gate = start.clone();
    let admitting = thread::spawn(move || { assert!(reserve_gate.wait()); reserving.reserve(&fence("racing")) });
    let closing = case.supervisor.clone(); let close_gate = start.clone();
    let closer = thread::spawn(move || { assert!(close_gate.wait()); closing.try_close_off_app() });
    start.open();
    let admitted = admitting.join().expect("reserve caller panicked");
    let closed = closer.join().expect("close caller panicked");
    match (admitted, closed) {
        (Ok(reservation), Err(actual)) => {
            assert!(Arc::ptr_eq(&actual, &case.supervisor));
            assert!(case.supervisor.reserve(&fence("late")).is_err());
            case.reservations.push(Some(reservation)); drop(actual);
        }
        (Err(_), Ok(proof)) => { assert!(!proof.join_worker_panicked); case.closed = true; }
        (Ok(reservation), Ok(_)) => {
            case.reservations.push(Some(reservation)); panic!("close reported stopped with admitted source custody");
        }
        (Err(_), Err(actual)) => { drop(actual); panic!("idle admission/close race lost both operations"); }
    }
    case.finish(); assert!(case.closed);
}
