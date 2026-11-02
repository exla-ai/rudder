//! Feature-only historical Root command DATA. No effect/clock/owner issuer.
//! The RootHostControl collection owns physical reporters independently of all
//! pending/orphan/known-receipt moves and bounded receipt-history eviction.
use super::*;
use rudder_engine::execution::handoff::{OwnedWorkerJoin, WorkerJoined};
use std::{
    any::Any,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{atomic::AtomicU8, Mutex, OnceLock},
};

const MAX_REPORTERS: usize = 1; // One ordinary command per destructive genuine boot.
const MAX_DATA_BYTES: usize = 4096;
const WAITING: u8 = 0;
const ARMED: u8 = 1;
const KNOWN_FORWARDED: u8 = 2;
const UNKNOWN_SEND: u8 = 3;
const CONSTRAINT_REFUSED: u8 = 4;
const CONTROL_REFUSED: u8 = 5;
const NO_SAMPLE: u8 = 0;
const SAMPLE_LIVE: u8 = 1;
const SAMPLE_REFUSED: u8 = 2;
const PREFIX: &[u8] = b"RUDDER_GENUINE_ROOT_COMMAND_DATA_V1=";

fn fixed_case() -> Result<&'static str> {
    let cases = [
        cfg!(feature = "managed-ingress-fixture-close-wins"),
        cfg!(feature = "managed-ingress-fixture-admission-wins"),
        cfg!(feature = "managed-ingress-fixture-carrier-contention"),
    ];
    ensure!(
        cases.iter().filter(|v| **v).count() == 1,
        "Root command DATA requires one genuine fixture case"
    );
    Ok(if cases[0] {
        "CloseWins"
    } else if cases[1] {
        "AdmissionWins"
    } else {
        "CarrierContention"
    })
}
struct OriginalSnapshot {
    command: HostCommand,
    bytes: Vec<u8>,
}
impl OriginalSnapshot {
    fn capture(pending: &RootPending) -> Result<Self> {
        ensure!(
            !pending.forwarded && pending.reply.is_none(),
            "Root receipt capture is before original forward only"
        );
        ensure!(
            !pending.bytes.is_empty() && pending.bytes.len() <= MAX_HOST_FRAME,
            "Root original packet bound"
        );
        pending.command.validate()?;
        ensure!(
            serde_json::from_slice::<HostCommand>(&pending.bytes)? == pending.command,
            "Root parsed/original packet disagreement"
        );
        fixed_case()?;
        Ok(Self {
            command: pending.command.clone(),
            bytes: pending.bytes.clone(),
        })
    }
}
struct Shared {
    original: OnceLock<OriginalSnapshot>,
    outcome: AtomicU8,
    sample: AtomicU8,
    attempt_started: AtomicBool,
    pre_forward_sample_live: AtomicBool,
    owned: AtomicBool,
    // A worker can own Shared, never ReceiptOwner/OwnedWorkerJoin/RootControl.
    report_error: Mutex<Option<anyhow::Error>>,
    report_panic: Mutex<Option<Box<dyn Any + Send>>>,
}
impl Shared {
    fn new() -> Self {
        Self {
            original: OnceLock::new(),
            outcome: AtomicU8::new(WAITING),
            sample: AtomicU8::new(NO_SAMPLE),
            attempt_started: AtomicBool::new(false),
            pre_forward_sample_live: AtomicBool::new(false),
            owned: AtomicBool::new(false),
            report_error: Mutex::new(None),
            report_panic: Mutex::new(None),
        }
    }
    fn refuse_waiting(&self, value: u8) {
        let _ = self
            .outcome
            .compare_exchange(WAITING, value, Ordering::AcqRel, Ordering::Acquire);
    }
}
/// Only an alias to DATA captured from SAME RootPending. No live owner, effect
/// callback, constructor from JSON, original budget or join is exposed here.
pub(super) struct ReceiptHandle {
    shared: Arc<Shared>,
}
impl ReceiptHandle {
    pub(super) fn observe_original_constraint_sample(&self, live: bool) {
        self.shared.sample.store(
            if live { SAMPLE_LIVE } else { SAMPLE_REFUSED },
            Ordering::Release,
        );
        if !live {
            self.shared.refuse_waiting(CONSTRAINT_REFUSED);
        }
    }
    pub(super) fn arm_original_forward(&self) -> Option<OriginalForwardAttempt> {
        if self.shared.original.get().is_none() {
            return None;
        }
        if self
            .shared
            .outcome
            .compare_exchange(WAITING, ARMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        self.shared.pre_forward_sample_live.store(
            self.shared.sample.load(Ordering::Acquire) == SAMPLE_LIVE,
            Ordering::Release,
        );
        self.shared.attempt_started.store(true, Ordering::Release);
        Some(OriginalForwardAttempt {
            shared: self.shared.clone(),
            armed: true,
        })
    }
}
/// Armed BEFORE the existing send. Unwinding/unknown result never turns into
/// known-not-sent DATA. This guard cannot send, cancel, retry or extend time.
pub(super) struct OriginalForwardAttempt {
    shared: Arc<Shared>,
    armed: bool,
}
impl OriginalForwardAttempt {
    pub(super) fn observe_original_result(mut self, result: &Result<bool>) {
        let next = match result {
            Ok(true) => KNOWN_FORWARDED,
            Ok(false) => WAITING,
            Err(_) => UNKNOWN_SEND,
        };
        let _ =
            self.shared
                .outcome
                .compare_exchange(ARMED, next, Ordering::AcqRel, Ordering::Acquire);
        self.armed = false;
    }
}
impl Drop for OriginalForwardAttempt {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.shared.outcome.compare_exchange(
                ARMED,
                UNKNOWN_SEND,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

struct ReceiptOwner {
    shared: Arc<Shared>,
    worker: Option<OwnedWorkerJoin>,
    joined: Option<WorkerJoined>,
    spawn_attempted: bool,
    join_observer_attempted: bool,
    capture_error: Option<anyhow::Error>,
    spawn_error: Option<std::io::Error>,
    arm_panic: Option<Box<dyn Any + Send>>,
    join_error: Option<anyhow::Error>,
    join_panic: Option<Box<dyn Any + Send>>,
}
impl ReceiptOwner {
    fn new() -> Self {
        Self {
            shared: Arc::new(Shared::new()),
            worker: None,
            joined: None,
            spawn_attempted: false,
            join_observer_attempted: false,
            capture_error: None,
            spawn_error: None,
            arm_panic: None,
            join_error: None,
            join_panic: None,
        }
    }
    fn capture_and_start_once(&mut self, pending: &RootPending) {
        let armed = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let original = OriginalSnapshot::capture(pending)?;
            self.shared
                .original
                .set(original)
                .map_err(|_| anyhow::anyhow!("Root original snapshot already occupied"))?;
            ensure!(!self.spawn_attempted, "Root reporter cannot replay");
            self.spawn_attempted = true;
            let actual = self.shared.clone();
            match std::thread::Builder::new()
                .name("rudder-root-command-data".into())
                .spawn(move || {
                    while !actual.owned.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    let result = catch_unwind(AssertUnwindSafe(|| emit_original_data(&actual)));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            *actual
                                .report_error
                                .lock()
                                .unwrap_or_else(|p| p.into_inner()) = Some(error)
                        }
                        Err(panic) => {
                            *actual
                                .report_panic
                                .lock()
                                .unwrap_or_else(|p| p.into_inner()) = Some(panic)
                        }
                    }
                }) {
                Ok(worker) => {
                    self.worker = Some(OwnedWorkerJoin::new(worker));
                    self.shared.owned.store(true, Ordering::Release);
                }
                Err(error) => {
                    self.spawn_error = Some(error);
                }
            }
            Ok(())
        }));
        match armed {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.capture_error = Some(error),
            Err(panic) => self.arm_panic = Some(panic),
        }
        // In particular a panic without a stored JoinHandle proves no absence.
        // SAME owner/shared/partial fields stay retained; no retry or ready.
    }
    fn poll_actual_join(&mut self) {
        if self.joined.is_some() || self.join_error.is_some() || self.join_panic.is_some() {
            return;
        }
        let Some(worker) = self.worker.clone() else {
            return;
        };
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            if !self.join_observer_attempted {
                self.join_observer_attempted = true;
                worker.start_join_observer()?;
            }
            self.joined = worker.try_joined()?;
            Ok(())
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.join_error = Some(error),
            Err(panic) => self.join_panic = Some(panic),
        }
        // No recursive observer of this observer, App join, is_finished proof,
        // dropped handle, or retry after observation spawn/error/panic.
    }
}
#[must_use = "retain all original Root receipt reporters through whole-owner teardown"]
pub(super) struct ReceiptOwners {
    entries: Vec<ReceiptOwner>,
    capacity_refused: bool,
}
impl ReceiptOwners {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
            capacity_refused: false,
        }
    }
    fn retain_original_slot(&mut self, pending: &mut RootPending) -> Option<usize> {
        if pending.fixture_receipt.is_some()
            || !matches!(
                &pending.command.operation,
                HostOperation::OpenPreparedProjectV1 { .. }
            )
        {
            return None;
        }
        if self.entries.len() >= MAX_REPORTERS {
            self.capacity_refused = true;
            return None;
        }
        // Collection and SAME pending alias are retained BEFORE any fallible
        // snapshot/serialization/spawn. Moving/dropping pending cannot lose it.
        self.entries.push(ReceiptOwner::new());
        let original = self.entries.last_mut().unwrap();
        pending.fixture_receipt = Some(ReceiptHandle {
            shared: original.shared.clone(),
        });
        Some(self.entries.len() - 1)
    }
    pub(super) fn capture_original_retaining(&mut self, pending: &mut RootPending) {
        if let Some(index) = self.retain_original_slot(pending) {
            self.entries[index].capture_and_start_once(pending);
        }
    }
    pub(super) fn poll_actual_join_observations(&mut self) {
        for original in &mut self.entries {
            original.poll_actual_join();
        }
    }
    pub(super) fn observe_original_control_refusal(&self) {
        for original in &self.entries {
            original.shared.refuse_waiting(CONTROL_REFUSED);
        }
    }
}

#[derive(Serialize)]
struct RootCommandData<'a> {
    schema: u8,
    case: &'static str,
    canonical_command_sha256: String,
    raw_packet_sha256: String,
    raw_packet_bytes: usize,
    command_key_sha256: String,
    rpc_id_sha256: String,
    operation_sha256: String,
    session_generation_sha256: String,
    constraint: &'a FinalizationConstraint,
    original_forward_outcome: &'static str,
    original_send_attempt_armed: bool,
    original_pre_forward_sample_live: bool,
    root_known_forwarded: bool,
    reporting_worker_pending: bool,
    source_accepted: bool,
    runtime_ready: bool,
    retirement_complete: bool,
}
fn encoded_data(shared: &Shared, outcome: u8) -> Result<Vec<u8>> {
    let original = shared
        .original
        .get()
        .context("Root original command snapshot absent")?;
    let classification = match outcome {
        KNOWN_FORWARDED => "KnownForwarded",
        UNKNOWN_SEND => "UnknownOriginalSend",
        CONSTRAINT_REFUSED => "ConstraintRefusedBeforeKnownForward",
        CONTROL_REFUSED => "ControlRefusedBeforeKnownForward",
        _ => anyhow::bail!("Root original forwarding still pending"),
    };
    let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let data = RootCommandData {
        schema: 1,
        case: fixed_case()?,
        canonical_command_sha256: hash(&canonical_json(&original.command)?),
        raw_packet_sha256: hash(&original.bytes),
        raw_packet_bytes: original.bytes.len(),
        command_key_sha256: hash(original.command.command_key.as_bytes()),
        rpc_id_sha256: hash(original.command.rpc_id.as_bytes()),
        operation_sha256: hash(&canonical_json(&original.command.operation)?),
        session_generation_sha256: hash(original.command.session_generation.as_bytes()),
        constraint: &original.command.constraint,
        original_forward_outcome: classification,
        original_send_attempt_armed: shared.attempt_started.load(Ordering::Acquire),
        original_pre_forward_sample_live: shared.pre_forward_sample_live.load(Ordering::Acquire),
        root_known_forwarded: outcome == KNOWN_FORWARDED,
        reporting_worker_pending: true,
        source_accepted: false,
        runtime_ready: false,
        retirement_complete: false,
    };
    let raw = serde_json::to_vec(&data)?;
    ensure!(raw.len() <= MAX_DATA_BYTES, "Root fixed command DATA bound");
    Ok(raw)
}
fn emit_original_data(shared: &Shared) -> Result<()> {
    let outcome = loop {
        let actual = shared.outcome.load(Ordering::Acquire);
        if !matches!(actual, WAITING | ARMED) {
            break actual;
        }
        std::thread::yield_now();
    };
    let raw = encoded_data(shared, outcome)?;
    // Only this separately retained worker touches output. No Root transport,
    // payload, storage/carrier mutex is acquired by this path. Never retry IO.
    use std::io::Write;
    let mut output = std::io::stderr().lock();
    output.write_all(PREFIX)?;
    output.write_all(&raw)?;
    output.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // These plain payload fixtures qualify neither RootHostControl capture nor
    // genuine ingress. No command is sent and no Root DATA frame is emitted.
    fn unqualified_pending_data() -> RootPending {
        let constraint=FinalizationConstraint::from_json(br#"{"format_version":1,"maximum_milliseconds":15000,"boottime_deadline_nanoseconds":"123456789"}"#).unwrap();
        let command = HostCommand {
            version: 1,
            session_generation: "11111111-1111-4111-8111-111111111111".into(),
            rpc_id: "rpc-original".into(),
            command_key: "command-original".into(),
            operation: HostOperation::OpenPreparedProjectV1 {
                project_alias: "project-original".into(),
                backend: HostBackend::Codex,
                model: "model-private".into(),
                profile_id: "profile-private".into(),
            },
            constraint,
        };
        command.validate().unwrap();
        let mut bytes = vec![b' ', b'\n'];
        bytes.extend(canonical_json(&command).unwrap());
        RootPending {
            command,
            bytes,
            forwarded: false,
            reply: None,
            sidecar_instance: "sidecar-original".into(),
            fixture_receipt: None,
        }
    }
    fn captured_data() -> (RootPending, ReceiptHandle) {
        let pending = unqualified_pending_data();
        let shared = Arc::new(Shared::new());
        assert!(shared
            .original
            .set(OriginalSnapshot::capture(&pending).unwrap())
            .is_ok());
        (pending, ReceiptHandle { shared })
    }
    #[test]
    fn original_packet_and_constraint_projection_are_exact_and_non_authoritative() {
        let (pending, handle) = captured_data();
        handle.observe_original_constraint_sample(true);
        handle
            .arm_original_forward()
            .unwrap()
            .observe_original_result(&Ok(true));
        let raw = encoded_data(&handle.shared, KNOWN_FORWARDED).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        assert_eq!(value["raw_packet_sha256"], hash(&pending.bytes));
        assert_eq!(
            value["canonical_command_sha256"],
            hash(&canonical_json(&pending.command).unwrap())
        );
        assert_ne!(
            value["raw_packet_sha256"],
            value["canonical_command_sha256"]
        );
        assert_eq!(
            value["constraint"],
            serde_json::to_value(&pending.command.constraint).unwrap()
        );
        assert_eq!(value["raw_packet_bytes"], pending.bytes.len());
        for key in [
            "original_send_attempt_armed",
            "original_pre_forward_sample_live",
            "root_known_forwarded",
            "reporting_worker_pending",
        ] {
            assert_eq!(value[key], true);
        }
        for key in ["source_accepted", "runtime_ready", "retirement_complete"] {
            assert_eq!(value[key], false);
        }
        assert_eq!(value.as_object().unwrap().len(), 18);
        let text = String::from_utf8(raw).unwrap();
        for private in [
            "command-original",
            "rpc-original",
            "project-original",
            "model-private",
            "profile-private",
            "11111111-1111-4111-8111-111111111111",
        ] {
            assert!(!text.contains(private));
        }
        assert!(text.len() <= MAX_DATA_BYTES);
    }
    #[test]
    fn original_snapshot_refuses_packet_disagreement_and_post_forward_capture() {
        let mut pending = unqualified_pending_data();
        pending.command.command_key = "different-original".into();
        assert!(OriginalSnapshot::capture(&pending).is_err());
        pending = unqualified_pending_data();
        pending.forwarded = true;
        assert!(OriginalSnapshot::capture(&pending).is_err());
        pending = unqualified_pending_data();
        pending.bytes.resize(MAX_HOST_FRAME + 1, b' ');
        assert!(OriginalSnapshot::capture(&pending).is_err());
    }
    #[test]
    fn unknown_original_send_is_absorbing_after_unwind_or_error() {
        for unwind in [false, true] {
            let (_, handle) = captured_data();
            handle.observe_original_constraint_sample(true);
            if unwind {
                let observed = catch_unwind(AssertUnwindSafe(|| {
                    let _attempt = handle.arm_original_forward().unwrap();
                    panic!("unqualified send window test");
                }));
                assert!(observed.is_err());
            } else {
                handle
                    .arm_original_forward()
                    .unwrap()
                    .observe_original_result(&Err(anyhow::anyhow!("unqualified send failure")));
            }
            handle.observe_original_constraint_sample(false);
            handle.shared.refuse_waiting(CONTROL_REFUSED);
            assert_eq!(handle.shared.outcome.load(Ordering::Acquire), UNKNOWN_SEND);
            assert!(handle.arm_original_forward().is_none());
            let value: serde_json::Value =
                serde_json::from_slice(&encoded_data(&handle.shared, UNKNOWN_SEND).unwrap())
                    .unwrap();
            assert_eq!(value["original_forward_outcome"], "UnknownOriginalSend");
            assert_eq!(value["root_known_forwarded"], false);
        }
    }
    #[test]
    fn known_unsent_preserves_original_poll_and_known_forward_cannot_be_reclassified() {
        let (_, handle) = captured_data();
        handle.observe_original_constraint_sample(true);
        handle
            .arm_original_forward()
            .unwrap()
            .observe_original_result(&Ok(false));
        assert_eq!(handle.shared.outcome.load(Ordering::Acquire), WAITING);
        assert!(encoded_data(&handle.shared, WAITING).is_err());
        // A second arm stands for baseline's existing known-unsent poll only;
        // receipt code never sends or retries, and creates no new reporter.
        handle
            .arm_original_forward()
            .unwrap()
            .observe_original_result(&Ok(true));
        handle.observe_original_constraint_sample(false);
        handle.shared.refuse_waiting(CONTROL_REFUSED);
        assert_eq!(
            handle.shared.outcome.load(Ordering::Acquire),
            KNOWN_FORWARDED
        );
        assert!(handle
            .shared
            .pre_forward_sample_live
            .load(Ordering::Acquire));
        assert!(handle.arm_original_forward().is_none());
    }
    #[test]
    fn constraint_or_control_refusal_remains_distinct_from_known_forward() {
        for constraint_refusal in [false, true] {
            let (_, handle) = captured_data();
            if constraint_refusal {
                handle.observe_original_constraint_sample(false);
            } else {
                handle.shared.refuse_waiting(CONTROL_REFUSED);
            }
            let outcome = handle.shared.outcome.load(Ordering::Acquire);
            let value: serde_json::Value =
                serde_json::from_slice(&encoded_data(&handle.shared, outcome).unwrap()).unwrap();
            assert_eq!(
                value["original_forward_outcome"],
                if constraint_refusal {
                    "ConstraintRefusedBeforeKnownForward"
                } else {
                    "ControlRefusedBeforeKnownForward"
                }
            );
            assert_eq!(value["original_send_attempt_armed"], false);
            assert_eq!(value["root_known_forwarded"], false);
            assert!(handle.arm_original_forward().is_none());
        }
    }
    #[test]
    fn root_collection_retains_same_slot_after_pending_history_moves_and_capacity_refusal() {
        let mut owners = ReceiptOwners::new();
        let mut pending = unqualified_pending_data();
        let index = owners.retain_original_slot(&mut pending).unwrap();
        assert_eq!(index, 0);
        let same = Arc::downgrade(&owners.entries[0].shared);
        assert!(Arc::ptr_eq(
            &pending.fixture_receipt.as_ref().unwrap().shared,
            &same.upgrade().unwrap()
        ));
        // Exercise SAME payload carrier movement, without constructing a Root
        // transport or claiming these containers are actual original history.
        let mut orphans = vec![pending];
        let mut known = std::collections::VecDeque::new();
        known.push_back(orphans.remove(0));
        drop(known.pop_front());
        assert!(same.upgrade().is_some());
        assert_eq!(owners.entries.len(), 1);
        let mut later = unqualified_pending_data();
        later.command.command_key = "later-command".into();
        assert!(owners.retain_original_slot(&mut later).is_none());
        assert!(owners.capacity_refused);
        assert!(later.fixture_receipt.is_none());
        assert!(Arc::ptr_eq(
            &owners.entries[0].shared,
            &same.upgrade().unwrap()
        ));
        assert!(!owners.entries[0].spawn_attempted);
        assert!(owners.entries[0].worker.is_none());
    }
    #[test]
    fn actual_reporter_owner_requires_physical_worker_join_including_held_tls() {
        use std::{cell::RefCell, sync::mpsc};
        struct HeldTls {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for HeldTls {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
        }
        thread_local! {static HELD:RefCell<Option<HeldTls>>=const{RefCell::new(None)};}
        struct ReleaseOnDrop(Option<mpsc::Sender<()>>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        // Actual physical worker/TLS issuer, explicitly not a Root capture or
        // a genuine reporter frame. It returns from its body before TLS ends.
        let worker = std::thread::spawn(move || {
            HELD.with(|slot| {
                *slot.borrow_mut() = Some(HeldTls {
                    entered: entered_tx,
                    release: release_rx,
                })
            })
        });
        let mut original = ReceiptOwner::new();
        original.worker = Some(OwnedWorkerJoin::new(worker));
        original.poll_actual_join();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        original.poll_actual_join();
        assert!(original.joined.is_none());
        assert!(original.join_error.is_none());
        release.0.take().unwrap().send(()).unwrap();
        let actual = original
            .worker
            .as_ref()
            .unwrap()
            .await_joined(Duration::from_secs(5))
            .unwrap();
        original.poll_actual_join();
        assert!(original.joined.as_ref().unwrap().same_worker(&actual));
    }
}
