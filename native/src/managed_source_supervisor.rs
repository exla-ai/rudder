// Fixed cleanup/join custody, never a source scheduler. Included inside linux::supervision.
use std::sync::{Mutex, MutexGuard, TryLockError, atomic::{AtomicU64, AtomicU8}};
const RESERVED: u8 = 0;
const WORKING: u8 = 1;
const READY: u8 = 2;
const APP_OWNS: u8 = 3;
const FREE: u8 = 4;

fn held<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn available<T>(value: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match value.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}
fn unsent<T>(error: mpsc::TrySendError<T>) -> T {
    match error { mpsc::TrySendError::Full(value) | mpsc::TrySendError::Disconnected(value) => value }
}
#[derive(Clone)]
pub(crate) struct SourceSupervisionKey {
    instance: Arc<()>, slot: usize, generation: u64, fence: ManagedLaunchFence,
}
pub(crate) struct SourceSupervision { shared: Arc<Shared>, key: SourceSupervisionKey }
impl SourceSupervision {
    pub(crate) fn key(&self) -> SourceSupervisionKey { self.key.clone() }
    pub(crate) fn matches(&self, other: &SourceSupervisionKey) -> bool { same(&self.key, other) }
    pub(crate) fn cancel_before_effects(self) { self.release(None, true); }
    pub(crate) fn release_settled(self, live: Option<Live>) { self.release(live, false); }
    fn release(self, live: Option<Live>, no_effects: bool) {
        let packet = Normal::Release { key: self.key.clone(), live, no_effects, shutdown_only: false };
        // This unique reservation has no further App owner after release. If
        // ordinary ingress is occupied, its dedicated final inbox owns it.
        if let Err(error) = self.shared.slots[self.key.slot].normal_send.try_send(packet) {
            put_final(&self.shared, self.key.slot, Final::Release(unsent(error)));
        }
    }
}
fn same(a: &SourceSupervisionKey, b: &SourceSupervisionKey) -> bool {
    Arc::ptr_eq(&a.instance, &b.instance) && a.slot == b.slot
        && a.generation == b.generation && a.fence == b.fence
}
struct Slot {
    claimed: AtomicBool,
    generation: AtomicU64,
    phase: AtomicU8,
    normal_send: SyncSender<Normal>, normal_receive: Mutex<Receiver<Normal>>,
    final_send: SyncSender<Final>, final_receive: Mutex<Receiver<Final>>,
    return_send: SyncSender<SourceOwnerJoined>, return_receive: Mutex<Receiver<SourceOwnerJoined>>,
    stored: Mutex<Stored>,
}
impl Slot {
    fn new() -> Self {
        let (normal_send, normal_receive) = mpsc::sync_channel(1);
        let (final_send, final_receive) = mpsc::sync_channel(1);
        let (return_send, return_receive) = mpsc::sync_channel(1);
        Self { claimed: AtomicBool::new(false), generation: AtomicU64::new(0), phase: AtomicU8::new(FREE),
            normal_send, normal_receive: Mutex::new(normal_receive), final_send,
            final_receive: Mutex::new(final_receive), return_send,
            return_receive: Mutex::new(return_receive), stored: Mutex::new(Stored::default()) }
    }
}
#[derive(Default)]
struct Stored {
    key: Option<SourceSupervisionKey>,
    result: Option<SourceOwnerJoined>,
    recovery: Option<SourceRecoveryOwner>,
    live: Option<Live>,
    checkpoint: Option<JoinedSourceCheckpoint>,
    settled: bool, busy: bool, shutdown: bool, result_taken: bool,
    helper_writer_drain_proved: bool,
    fault: Option<&'static str>,
    join_failure: Option<Box<dyn std::any::Any + Send>>,
}
const CLOSING: usize = 1 << (usize::BITS - 1);
struct Shared { instance: Arc<()>, slots: [Slot; LIMIT], admission: AtomicUsize,
    stop: AtomicBool, join_panicked: AtomicBool }
enum Normal {
    Join(SourceOwnerJoinHandoff), Retry(SourceRecoveryHandoff),
    Release { key: SourceSupervisionKey, live: Option<Live>, no_effects: bool, shutdown_only: bool },
}
enum Final { Shutdown(SourceSlotShutdown), Release(Normal) }

/// A unique SourceSupervision is consumed by exactly ONE shutdown/release. Its
/// final inbox starts empty and is never used for ordinary jobs. Receivers live
/// in Shared, not the worker stack, so worker failure cannot disconnect it.
/// No allocation, wait, thread spawn, user callback or IO occurs on this path.
fn put_final(shared: &Shared, slot: usize, packet: Final) {
    match shared.slots[slot].final_send.try_send(packet) {
        Ok(()) => {}
        Err(_) => unreachable!("unique source final reservation used twice or released before handoff"),
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceSupervisorStatus {
    pub occupied: bool, pub busy: bool, pub result_ready: bool, pub recovery_retained: bool,
    pub shutdown: bool, pub helper_writer_drain_proved: bool, pub fault: Option<&'static str>,
}
#[must_use = "keep supervisor and Engine ownership until try_close_off_app returns its stopped proof"]
pub(crate) struct SourceOwnerLifetimeSupervisor {
    shared: Arc<Shared>, workers: Mutex<[Option<thread::JoinHandle<()>>; LIMIT]>,
}
pub(crate) struct StoppedSourceSupervisor { pub join_worker_panicked: bool }
impl Drop for SourceOwnerLifetimeSupervisor {
    fn drop(&mut self) {
        // Idle construction/early setup failure cannot leave polling threads.
        // Occupied supervisors must instead be retained from try_close_off_app:
        // only Root's lifetime owner may resolve that explicit Busy ownership.
        if self.shared.admission.load(Ordering::Acquire) & !CLOSING == 0 {
            self.shared.stop.store(true, Ordering::Release);
        }
    }
}
impl SourceOwnerLifetimeSupervisor {
    /// OFF-APP. Starts exactly two join workers before any source is admitted.
    pub(crate) fn new_off_app() -> Result<Arc<Self>> {
        let shared = Arc::new(Shared { instance: Arc::new(()), slots: std::array::from_fn(|_|Slot::new()),
            admission: AtomicUsize::new(0), stop: AtomicBool::new(false), join_panicked: AtomicBool::new(false) });
        let mut workers: [Option<thread::JoinHandle<()>>; LIMIT] = std::array::from_fn(|_|None);
        for index in 0..LIMIT {
            let owned = shared.clone();
            match thread::Builder::new().name(format!("rudder-source-join-{index}")).spawn(move || worker(owned, index)) {
                Ok(handle) => workers[index] = Some(handle),
                Err(error) => {
                    shared.stop.store(true, Ordering::Release);
                    for worker in workers.iter_mut().filter_map(Option::take) { let _ = worker.join(); }
                    return Err(error.into()); // No source could have been admitted.
                }
            }
        }
        Ok(Arc::new(Self { shared, workers: Mutex::new(workers) }))
    }
    /// Pure bounded admission across all Apps sharing this engine lifetime.
    pub(crate) fn reserve(&self, fence: &ManagedLaunchFence) -> Result<SourceSupervision> {
        self.shared.admission.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |value| (value & CLOSING == 0 && value < LIMIT).then_some(value + 1))
            .map_err(|_|anyhow::anyhow!("source supervisor is stopping or both lifetime slots remain owned"))?;
        for (index, slot) in self.shared.slots.iter().enumerate() {
            if slot.claimed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() { continue; }
            let Some(mut stored) = available(&slot.stored) else {
                slot.claimed.store(false, Ordering::Release); continue;
            };
            let generation = match slot.generation.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n|n.checked_add(1)) {
                Ok(previous) => previous + 1,
                Err(_) => { slot.claimed.store(false, Ordering::Release);
                    self.shared.admission.fetch_sub(1, Ordering::AcqRel);
                    anyhow::bail!("source supervisor generation exhausted"); }
            };
            let key = SourceSupervisionKey { instance: self.shared.instance.clone(), slot: index,
                generation, fence: fence.clone() };
            *stored = Stored { key: Some(key.clone()), ..Stored::default() };
            slot.phase.store(RESERVED, Ordering::Release);
            return Ok(SourceSupervision { shared: self.shared.clone(), key });
        }
        self.shared.admission.fetch_sub(1, Ordering::AcqRel);
        anyhow::bail!("both source lifetime slots are occupied, including recovery or TLS teardown")
    }
    fn current(&self, key: &SourceSupervisionKey) -> bool {
        Arc::ptr_eq(&key.instance, &self.shared.instance) && key.slot < LIMIT
            && self.shared.slots[key.slot].claimed.load(Ordering::Acquire)
            && self.shared.slots[key.slot].generation.load(Ordering::Acquire) == key.generation
    }
    /// Refused ordinary admission returns the original actual handle/capsule.
    pub(crate) fn try_join(&self, job: SourceOwnerJoinHandoff) -> std::result::Result<(), SourceOwnerJoinHandoff> {
        if !self.current(&job.supervision) || job.fence != job.supervision.fence { return Err(job); }
        let slot = &self.shared.slots[job.supervision.slot];
        if slot.phase.compare_exchange(RESERVED, WORKING, Ordering::AcqRel, Ordering::Acquire).is_err() { return Err(job); }
        match slot.normal_send.try_send(Normal::Join(job)) {
            Ok(()) => Ok(()), Err(error) => { slot.phase.store(RESERVED, Ordering::Release);
                match unsent(error) { Normal::Join(job) => Err(job), _ => unreachable!() } }
        }
    }
    pub(crate) fn try_retry_unused(&self, job: SourceRecoveryHandoff) -> std::result::Result<(), SourceRecoveryHandoff> {
        if !self.current(&job.supervision) || job.fence != job.supervision.fence
            || job.owner.thread_settlement_failed || job.owner.carrier.adopted || job.owner.carrier.may_have_helper { return Err(job); }
        let slot = &self.shared.slots[job.supervision.slot];
        if slot.phase.compare_exchange(APP_OWNS, WORKING, Ordering::AcqRel, Ordering::Acquire).is_err() { return Err(job); }
        match slot.normal_send.try_send(Normal::Retry(job)) {
            Ok(()) => Ok(()), Err(error) => { slot.phase.store(APP_OWNS, Ordering::Release);
                match unsent(error) { Normal::Retry(job) => Err(job), _ => unreachable!() } }
        }
    }
    pub(crate) fn try_take_joined(&self) -> Option<SourceOwnerJoined> {
        for slot in &self.shared.slots {
            let Some(mut stored) = available(&slot.stored) else { continue; };
            if let Some(result) = stored.result.take() {
                stored.result_taken = true; slot.phase.store(APP_OWNS, Ordering::Release); return Some(result);
            }
        }
        None
    }
    /// Only returns the same non-Clone result just taken above. One dedicated
    /// return cell per slot is reserved for it, independent of job admission.
    pub(crate) fn retain_unadopted(&self, result: SourceOwnerJoined) {
        let slot = result.supervision.slot;
        match self.shared.slots[slot].return_send.try_send(result) {
            Ok(()) => {}
            Err(_) => unreachable!("source result returned twice or its lifetime was ended before adoption"),
        }
    }
    pub(crate) fn retain_shutdown(&self, handoff: SourceOwnerShutdownHandoff) {
        self.shared.admission.fetch_or(CLOSING, Ordering::AcqRel);
        for packet in handoff.slots.into_iter().flatten() {
            let slot = packet.supervision.key.slot;
            put_final(&self.shared, slot, Final::Shutdown(packet));
        }
    }
    pub(crate) fn status(&self) -> [SourceSupervisorStatus; LIMIT] {
        std::array::from_fn(|index| {
            let slot = &self.shared.slots[index];
            let stored = available(&slot.stored);
            SourceSupervisorStatus { occupied: slot.claimed.load(Ordering::Acquire),
                busy: stored.as_ref().is_none_or(|state|state.busy),
                result_ready: stored.as_ref().is_some_and(|state|state.result.is_some()),
                recovery_retained: stored.as_ref().is_some_and(|state|state.recovery.is_some()
                    || matches!(state.result.as_ref().map(|v|&v.result), Some(Ok(SourceOwnerExit::Recovery(_))))),
                shutdown: stored.as_ref().is_some_and(|state|state.shutdown),
                helper_writer_drain_proved: stored.as_ref().is_some_and(|state|state.helper_writer_drain_proved),
                fault: stored.as_ref().and_then(|state|state.fault) }
        })
    }
    pub(crate) fn keys(&self) -> [Option<SourceSupervisionKey>; LIMIT] {
        std::array::from_fn(|index|available(&self.shared.slots[index].stored).and_then(|stored|stored.key.clone()))
    }
    /// OFF-APP explicit retention/adoption after App is gone; payload is DATA,
    /// never publication or permission to release an unresolved source owner.
    pub(crate) fn try_take_shutdown_checkpoint(&self, key: &SourceSupervisionKey) -> Option<JoinedSourceCheckpoint> {
        if !self.current(key) { return None; }
        available(&self.shared.slots[key.slot].stored)?.checkpoint.take()
    }
    /// Admission only. Observe actual free/close proof before reporting release.
    pub(crate) fn request_release_shutdown_settled(&self, key: &SourceSupervisionKey) -> Result<()> {
        anyhow::ensure!(self.current(key), "source supervision identity changed");
        self.shared.slots[key.slot].normal_send.try_send(Normal::Release { key: key.clone(), live: None,
            no_effects: false, shutdown_only: true })
            .map_err(|_|anyhow::anyhow!("source release admission is busy"))
    }
    /// OFF-APP. Busy returns THIS actual supervisor, with every slot/capsule and
    /// worker still owned. Root must keep EngineOwnership too; never turn Busy
    /// into a forced owner finish or a claimed stopped/deployed result.
    pub(crate) fn try_close_off_app(self: Arc<Self>) -> std::result::Result<StoppedSourceSupervisor, Arc<Self>> {
        let previous = self.shared.admission.fetch_or(CLOSING, Ordering::AcqRel);
        if previous & !CLOSING != 0 { return Err(self); }
        self.shared.stop.store(true, Ordering::Release);
        let mut failed = false;
        for worker in held(&self.workers).iter_mut().filter_map(Option::take) { failed |= worker.join().is_err(); }
        if failed { self.shared.join_panicked.store(true, Ordering::Release); }
        Ok(StoppedSourceSupervisor { join_worker_panicked: self.shared.join_panicked.load(Ordering::Acquire) })
    }
}

fn publish(slot: &Slot, mut result: SourceOwnerJoined) {
    let mut stored = held(&slot.stored);
    // A raw panic payload can execute arbitrary Drop. Retain it here off App;
    // App only receives a unit error marker, never that destructor obligation.
    if stored.join_failure.is_none() {
        if let Err(payload) = &mut result.result {
            stored.join_failure = Some(std::mem::replace(payload, Box::new(())));
        }
    }
    stored.busy = false;
    stored.result_taken = false;
    let contradicts_live = matches!(&result.result, Ok(SourceOwnerExit::Settled { unused: true, .. })) && stored.live.is_some();
    stored.settled = matches!(&result.result, Ok(SourceOwnerExit::Settled { .. })) && !contradicts_live;
    stored.helper_writer_drain_proved = stored.settled;
    if stored.settled { stored.fault = None; }
    if let Ok(SourceOwnerExit::Recovery(owner)) = &result.result { stored.fault = Some(owner.detail); }
    if result.result.is_err() { stored.fault = Some("source actor escaped retained unwind boundary; custody unproved"); }
    if contradicts_live { stored.fault = Some("unused outcome contradicts retained helper custody; drain remains unproved"); }
    stored.result = Some(result);
    slot.phase.store(READY, Ordering::Release);
}
fn join_one(shared: &Arc<Shared>, index: usize, job: SourceOwnerJoinHandoff) {
    held(&shared.slots[index].stored).busy = true;
    publish(&shared.slots[index], job.join_off_app()); // Real join, including TLS, ONLY here.
}
fn retry_one(shared: &Arc<Shared>, index: usize, job: SourceRecoveryHandoff) {
    let key = job.supervision;
    {
        let mut stored = held(&shared.slots[index].stored);
        stored.busy = true; stored.recovery = Some(job.owner);
    }
    // The actual owner is already in stable slot storage before a fallible
    // retry spawn, and is only BORROWED across its own catch boundary.
    let owned = shared.clone();
    let spawn = thread::Builder::new().name("rudder-source-unused-retry".into()).spawn(move || {
        let mut stored = held(&owned.slots[index].stored);
        if let Some(owner) = stored.recovery.as_mut() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||owner.retry_unused_off_app()));
            if result.is_err() { owner.thread_settlement_failed = true; }
            if !matches!(result, Ok(Ok(()))) { owner.detail = "explicit unused cleanup retry did not settle; actual ownership retained"; }
        }
    });
    let (started, joined) = match spawn { Ok(worker) => (true, worker.join().is_ok()), Err(_) => (false, false) };
    let mut stored = held(&shared.slots[index].stored);
    if !joined { stored.fault = Some("unused retry spawn or TLS join failed; actual recovery remains owned"); }
    let mut owner = stored.recovery.take().expect("retry owner remains in its reserved slot");
    if started && !joined {
        owner.thread_settlement_failed = true;
        owner.detail = "unused recovery worker did not join successfully; actual custody retained without settlement proof";
    } else if !started {
        owner.detail = "unused retry thread did not start; actual owner remains available for explicit retry";
    }
    drop(stored);
    publish(&shared.slots[index], SourceOwnerJoined { fence: key.fence.clone(), supervision: key,
        result: Ok(owner.into_exit()) });
}
fn release(shared: &Shared, index: usize, key: SourceSupervisionKey, live: Option<Live>, no_effects: bool, shutdown_only: bool) {
    let slot = &shared.slots[index];
    let mut stored = held(&slot.stored);
    if !stored.key.as_ref().is_some_and(|known|same(known, &key)) { return; }
    if live.is_some() { stored.live = live; }
    if (shutdown_only && !stored.shutdown) || !(stored.settled || no_effects) || stored.busy || stored.recovery.is_some() || stored.result.is_some()
        || stored.checkpoint.is_some() || stored.join_failure.is_some() {
        stored.fault = Some("release refused; actual source/result/checkpoint ownership remains retained"); return;
    }
    // Last issuer/control/Journal destructors run off App. A shutdown with no
    // real proof never reaches this branch and cannot free the reservation.
    *stored = Stored::default();
    drop(stored);
    slot.phase.store(FREE, Ordering::Release);
    slot.claimed.store(false, Ordering::Release);
    shared.admission.fetch_sub(1, Ordering::AcqRel);
}
fn worker(shared: Arc<Shared>, index: usize) {
    let slot = &shared.slots[index];
    while !shared.stop.load(Ordering::Acquire) {
        if let Ok(final_packet) = held(&slot.final_receive).try_recv() {
            match final_packet {
                Final::Release(Normal::Release { key, live, no_effects, shutdown_only }) => release(&shared, index, key, live, no_effects, shutdown_only),
                Final::Release(_) => unreachable!(),
                Final::Shutdown(mut packet) => {
                    let key = packet.supervision.key();
                    {
                        let mut stored = held(&slot.stored);
                        stored.shutdown = true; stored.live = packet.live.take(); stored.checkpoint = packet.checkpoint.take();
                        if !stored.helper_writer_drain_proved { stored.fault = Some("App shutdown has not proved helper/writer drain; retained ownership only"); }
                    }
                    if let Some(worker) = packet.worker.take() {
                        join_one(&shared, index, SourceOwnerJoinHandoff { fence: key.fence.clone(), worker, supervision: key.clone() });
                    }
                    if let Some(owner) = packet.recovery.take() {
                        publish(slot, SourceOwnerJoined { fence: key.fence.clone(), supervision: key, result: Ok(SourceOwnerExit::Recovery(owner)) });
                    }
                }
            }
            continue;
        }
        if let Ok(result) = held(&slot.return_receive).try_recv() { publish(slot, result); continue; }
        if let Ok(packet) = held(&slot.normal_receive).try_recv() {
            match packet {
                Normal::Join(job) => join_one(&shared, index, job),
                Normal::Retry(job) => retry_one(&shared, index, job),
                Normal::Release { key, live, no_effects, shutdown_only } => release(&shared, index, key, live, no_effects, shutdown_only),
            }
            continue;
        }
        thread::sleep(Duration::from_millis(10)); // Fixed workers only; App never waits here.
    }
}

#[cfg(test)]
mod tests {
    include!("managed_source_supervisor_tests.rs");
}
