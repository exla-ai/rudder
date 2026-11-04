//! Actual threads and TLS destructors. No helper/backend exit is fabricated.
use super::*;
use std::{cell::RefCell, sync::atomic::{AtomicBool, AtomicUsize, Ordering}};

struct ExitGate {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    dropped: Arc<AtomicUsize>,
}
impl Drop for ExitGate {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv_timeout(Duration::from_secs(5));
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }
}
thread_local! { static EXIT_GATE: RefCell<Option<ExitGate>> = const { RefCell::new(None) }; }

fn held_worker(body: impl FnOnce() + Send + 'static) ->
    (std::thread::JoinHandle<()>, mpsc::Receiver<()>, mpsc::SyncSender<()>, Arc<AtomicUsize>) {
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let dropped = Arc::new(AtomicUsize::new(0)); let worker_dropped = dropped.clone();
    let worker = std::thread::spawn(move || {
        EXIT_GATE.with(|slot| *slot.borrow_mut() = Some(ExitGate { entered, release: release_rx, dropped: worker_dropped }));
        body();
    });
    (worker, entered_rx, release, dropped)
}

#[test]
fn message_completion_and_deadline_do_not_replace_actual_thread_tls_join() {
    let (sent, received) = mpsc::sync_channel(1);
    let (worker, entered, release, dropped) = held_worker(move || { sent.send(()).unwrap(); });
    let owner = OwnedWorkerJoin::new(worker);
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(owner.try_joined().unwrap().is_none()); // Pure query does not start a join.
    assert!(owner.await_joined(Duration::from_millis(20)).is_err());
    assert_eq!(dropped.load(Ordering::Acquire), 0);
    assert!(owner.try_joined().unwrap().is_none());
    release.send(()).unwrap();
    let joined = owner.await_joined(Duration::from_secs(1)).unwrap();
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let cached = owner.await_joined(Duration::from_secs(1)).unwrap();
    assert!(joined.same_worker(&cached));
    assert!(joined.same_worker(&owner.try_joined().unwrap().unwrap()));
}

struct PanicPayload(Arc<AtomicUsize>);
impl Drop for PanicPayload { fn drop(&mut self) { self.0.fetch_add(1, Ordering::AcqRel); } }

#[test]
fn original_panic_is_joined_through_tls_and_retained_as_one_cached_failure() {
    let payload_drops = Arc::new(AtomicUsize::new(0)); let payload = payload_drops.clone();
    let (worker, entered, release, tls_drops) = held_worker(move || std::panic::panic_any(PanicPayload(payload)));
    let owner = OwnedWorkerJoin::new(worker); let alias = owner.clone();
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(owner.await_joined(Duration::from_millis(20)).is_err());
    assert_eq!(tls_drops.load(Ordering::Acquire), 0);
    release.send(()).unwrap();
    assert!(owner.await_joined(Duration::from_secs(1)).is_err());
    assert_eq!(tls_drops.load(Ordering::Acquire), 1);
    assert!(alias.await_joined(Duration::from_secs(1)).is_err());
    assert!(owner.try_joined().is_err());
    assert_eq!(payload_drops.load(Ordering::Acquire), 0, "panic payload remains privately owned");
    drop(owner); drop(alias);
    let until = Instant::now() + Duration::from_secs(1);
    while payload_drops.load(Ordering::Acquire) == 0 && Instant::now() < until { std::thread::sleep(Duration::from_millis(1)); }
    assert_eq!(payload_drops.load(Ordering::Acquire), 1);
}

fn managed_pair(worker: OwnedWorkerJoin, result: mpsc::Receiver<std::result::Result<(), String>>) ->
    (ManagedExecutionHandle, HandoffReceipt) {
    let binding = readiness_tests::binding();
    let helper = Arc::new(helper_process::HelperProcessWitness::pending(
        pidfd(std::process::id()).unwrap(), std::process::id(), unsafe { libc::geteuid() }, binding.clone()));
    let closed = Arc::new(AtomicBool::new(false));
    let (commands, _) = mpsc::sync_channel(1); let (_, events) = mpsc::sync_channel(1);
    (ManagedExecutionHandle { helper_process: helper.clone(), binding, commands, events,
        pending: std::cell::RefCell::new(None), stop_v4: std::cell::RefCell::new(None),
        authenticated: std::cell::Cell::new(false), closed: closed.clone(), worker: worker.clone() },
     HandoffReceipt { helper_process: Some(helper), result, completed: std::cell::Cell::new(false), closed, worker })
}

#[test]
fn managed_control_and_receipt_concurrently_join_the_same_original_worker_once() {
    let (send, result) = mpsc::sync_channel(1);
    let (worker, entered, release, dropped) = held_worker(move || { send.send(Ok(())).unwrap(); });
    let owner = OwnedWorkerJoin::new(worker);
    let (mut control, mut receipt) = managed_pair(owner.clone(), result);
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(receipt.try_result(), Some(Ok(()))); // Descriptor completion precedes TLS exit.
    control.close(); receipt.close();
    assert!(control.closed.load(Ordering::Acquire));
    assert!(receipt.closed.load(Ordering::Acquire));
    let (joined_send, joined_receive) = mpsc::channel();
    let left_send = joined_send.clone();
    let left = std::thread::spawn(move || {
        let joined = control.await_closed(Duration::from_secs(2)).unwrap();
        left_send.send(()).unwrap(); (control, joined)
    });
    let right = std::thread::spawn(move || {
        let joined = receipt.await_closed(Duration::from_secs(2)).unwrap();
        joined_send.send(()).unwrap(); (receipt, joined)
    });
    assert!(joined_receive.recv_timeout(Duration::from_millis(30)).is_err());
    assert_eq!(dropped.load(Ordering::Acquire), 0);
    release.send(()).unwrap();
    let (_control, control_joined) = left.join().unwrap();
    let (_receipt, receipt_joined) = right.join().unwrap();
    assert!(control_joined.same_worker(&receipt_joined));
    assert!(control_joined.same_worker(&owner.try_joined().unwrap().unwrap()));
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert!(!exited(&pidfd(std::process::id()).unwrap()).unwrap(), "a worker join does not prove helper process exit");
}

#[test]
fn standalone_receipt_wait_borrows_and_keeps_original_worker_after_early_result() {
    let (send, result) = mpsc::sync_channel(1);
    let (worker, entered, release, dropped) = held_worker(move || { send.send(Ok(())).unwrap(); });
    let mut receipt = HandoffReceipt { helper_process: None, result, completed: std::cell::Cell::new(false),
        closed: Arc::new(AtomicBool::new(false)), worker: OwnedWorkerJoin::new(worker) };
    receipt.wait(Duration::from_secs(1)).unwrap();
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(receipt.await_closed(Duration::from_millis(20)).is_err());
    assert_eq!(dropped.load(Ordering::Acquire), 0);
    release.send(()).unwrap();
    let joined = receipt.await_closed(Duration::from_secs(1)).unwrap();
    assert!(joined.same_worker(&receipt.await_closed(Duration::from_secs(1)).unwrap()));
    assert_eq!(dropped.load(Ordering::Acquire), 1);
}

#[test]
fn worker_join_tokens_and_shared_custody_are_send_without_serialization() {
    fn send<T: Send + 'static>() {}
    send::<OwnedWorkerJoin>(); send::<WorkerJoined>();
}
