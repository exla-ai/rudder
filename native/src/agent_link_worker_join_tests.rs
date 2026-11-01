use super::*;
use std::{cell::RefCell, sync::atomic::AtomicUsize};

struct TlsGate { entered: mpsc::SyncSender<()>, release: Receiver<()>, finished: Arc<AtomicUsize> }
impl Drop for TlsGate {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv_timeout(Duration::from_secs(5));
        self.finished.fetch_add(1, Ordering::AcqRel);
    }
}
thread_local! { static TLS_GATE: RefCell<Option<TlsGate>> = const { RefCell::new(None) }; }

#[test]
fn closed_link_event_and_callback_do_not_substitute_for_original_actor_tls_join() {
    let (commands, _) = mpsc::sync_channel(1);
    let stopped = Arc::new(AtomicBool::new(false)); let worker_stopped = stopped.clone();
    let (closed, event) = mpsc::channel(); let (entered, entering) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let finished = Arc::new(AtomicUsize::new(0)); let worker_finished = finished.clone();
    let worker = std::thread::spawn(move || {
        TLS_GATE.with(|slot| *slot.borrow_mut() = Some(TlsGate { entered, release: released, finished: worker_finished }));
        worker_stopped.store(true, Ordering::Release);
        closed.send(AgentLinkEvent { run_id: "run".into(), attempt_id: "attempt".into(),
            kind: AgentLinkEventKind::Closed { reason: "fixture actor body returned".into() } }).unwrap();
    });
    let mut handle = AgentLinkHandle { run_id: "run".into(), attempt_id: "attempt".into(),
        commands, stopped, worker: OwnedWorkerJoin::new(worker) };
    assert!(matches!(event.recv_timeout(Duration::from_secs(1)).unwrap().kind, AgentLinkEventKind::Closed { .. }));
    entering.recv_timeout(Duration::from_secs(1)).unwrap();
    handle.close_callback()(); handle.close();
    assert!(handle.stopped.load(Ordering::Acquire));
    assert_eq!(finished.load(Ordering::Acquire), 0);
    assert!(handle.await_closed(Duration::from_millis(20)).is_err());
    assert_eq!(finished.load(Ordering::Acquire), 0);
    release.send(()).unwrap();
    let _joined = handle.await_closed(Duration::from_secs(1)).unwrap();
    let _cached = handle.await_closed(Duration::from_secs(1)).unwrap();
    assert_eq!(finished.load(Ordering::Acquire), 1);
}

#[test]
fn panicked_link_actor_remains_a_cached_error_and_never_returns_join_proof() {
    let (commands, _) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(|| panic!("fixture link actor panic"));
    let mut handle = AgentLinkHandle { run_id: "run".into(), attempt_id: "attempt".into(), commands,
        stopped: Arc::new(AtomicBool::new(false)), worker: OwnedWorkerJoin::new(worker) };
    assert!(handle.await_closed(Duration::from_secs(1)).is_err());
    assert!(handle.await_closed(Duration::from_secs(1)).is_err());
}

#[test]
fn link_join_token_can_remain_in_original_off_app_cleanup_carrier() {
    fn send<T: Send + 'static>() {}
    send::<AgentLinkHandle>(); send::<AgentLinkWorkerJoined>();
}
