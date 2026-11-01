// Uses the adjacent actual EngineOwnership/task admission fixture. No helper,
// Root ACK, seed/localized proof or successful never-started capsule is faked.
#[test]
fn never_started_preparation_receipt_is_born_only_after_actual_original_tls_join() {
    let f=Fixture::new();let task=f.slot(f.admission());let queue=WorkerQueue::<bool>::new();
    let (entered,saw)=mpsc::sync_channel(1);let (release,wait)=mpsc::sync_channel(1);
    queue.spawn_retaining(queue.admit().unwrap(),task.worker().unwrap(),None,move |_|{
        TLS_GATE.with(|tls|*tls.borrow_mut()=Some(TlsGate {entered,release:wait}));true
    },||false).unwrap();assert!(queue.receive.recv_timeout(Duration::from_secs(2)).unwrap());saw.recv_timeout(Duration::from_secs(2)).unwrap();
    f.supervisor.as_ref().unwrap().poll();
    assert!(task.slot.carrier.lock().unwrap().preparation_proof.is_none(),"callback/result is not actual TLS join");
    release.send(()).unwrap();f.pump(||task.slot.carrier.lock().unwrap().preparation_proof.is_some());
    task.unused();f.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);drop(task);f.finish();
}
#[test]
fn caught_preparation_panic_keeps_original_payload_even_when_worker_thread_actually_joins() {
    let f=Fixture::new();let task=f.slot(f.admission());let queue=WorkerQueue::<bool>::new();
    queue.spawn_retaining(queue.admit().unwrap(),task.worker().unwrap(),None,|_|panic!("original preparation panic"),||false).unwrap();
    assert!(!queue.receive.recv_timeout(Duration::from_secs(2)).unwrap());
    f.pump(||task.slot.carrier.lock().unwrap().preparation_proof.is_some());
    assert_eq!(task.slot.carrier.lock().unwrap().panics.len(),1,"catch must retain actual unknown-effect panic alongside original carrier");
    task.unused();f.pump(||task.slot.state.load(Ordering::Acquire)==TASK_SETTLED);drop(task);f.finish();
}
