// Real sockets, current authenticated fixture and original worker/TLS joins.
// No model call, changed admission limit or synthetic busy/settled response.
#[test]
fn cancel_admission_full_request_slots_queue_valid_probe_without_unconfirmed_eof() {
    let f=Fixture::new();let broker=f.broker();let mut held=Vec::new();
    for _ in 0..MAX_CONNECTIONS { held.push(UnixStream::connect(&f.socket).unwrap()); }
    let filled=Instant::now()+Duration::from_secs(1);
    while broker.inner.count.load(Ordering::Acquire)!=MAX_CONNECTIONS || broker.inner.connections.lock().unwrap().len()!=MAX_CONNECTIONS {
        assert!(Instant::now()<filled,"actual broker handlers did not fill fixed admission");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(broker.inner.connections.lock().unwrap().len(),MAX_CONNECTIONS);
    let deadline=Instant::now()+Duration::from_secs(3);
    let mut queued=connect_until(&f.socket,deadline,None).unwrap();
    queued.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    let id=uuid::Uuid::new_v4().to_string();
    write_frame(&mut queued,&Request {format_version:1,id:id.clone(),identity:f.a.clone(),operation:Operation::Authenticate},MAX_REQUEST).unwrap();
    let mut first=[0];
    let early=queued.read(&mut first);
    assert!(matches!(early,Err(ref error) if matches!(error.kind(),io::ErrorKind::WouldBlock|io::ErrorKind::TimedOut)),
        "a valid ninth request must stay queued, never be discarded as EOF: {early:?}");
    assert_eq!(broker.inner.count.load(Ordering::Acquire),MAX_CONNECTIONS);
    for stream in &held { stream.shutdown(std::net::Shutdown::Both).unwrap(); }drop(held);
    let bytes=read_frame(&mut queued,MAX_RESPONSE,deadline,||Ok(())).unwrap();
    let response:Response=serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response.id,id);assert!(response.error.is_none());
    assert_eq!(response.result.unwrap()["binding"],serde_json::to_value(broker.binding()).unwrap());
    assert!(broker.inner.count.load(Ordering::Acquire)<=MAX_CONNECTIONS);
    assert!(broker.inner.requests.handles().capacity()<=MAX_OWNED_UNJOINED);broker.close();
}

struct CancelAdmissionTls {entered:mpsc::Sender<()>,release:mpsc::Receiver<()>}
impl Drop for CancelAdmissionTls {fn drop(&mut self) { let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(5)); }}
thread_local! {static CANCEL_ADMISSION_TLS:std::cell::RefCell<Option<CancelAdmissionTls>>=const {std::cell::RefCell::new(None)};}
struct CancelAdmissionRelease(Option<mpsc::Sender<()>>);
impl CancelAdmissionRelease {fn open(&mut self) {self.0.take().unwrap().send(()).unwrap();}}
impl Drop for CancelAdmissionRelease {fn drop(&mut self) {if let Some(release)=self.0.take() {let _=release.send(());}}}
#[test]
fn cancel_admission_original_request_tls_retains_count_and_socket_until_actual_join() {
    let (mut peer,stream)=UnixStream::pair().unwrap();let retained=stream.try_clone().unwrap();
    let connections=Arc::new(Mutex::new(BTreeMap::from([(7,retained)])));let count=Arc::new(AtomicUsize::new(1));
    let calls=Arc::new(OwnedBrokerCalls::new());let (entered,wait)=mpsc::channel();let (release,blocked)=mpsc::channel();
    let mut release=CancelAdmissionRelease(Some(release));
    let worker=std::thread::spawn(move|| {let _stream=stream;CANCEL_ADMISSION_TLS.with(|slot|*slot.borrow_mut()=Some(CancelAdmissionTls {entered,release:blocked}));});
    wait.recv_timeout(Duration::from_secs(2)).unwrap(); // callback returned, actual TLS still blocked
    assert!(calls.track_connection(worker,ConnectionAdmission {id:7,connections:connections.clone(),count:count.clone()}));
    let joining=calls.clone();let (done,wait_done)=mpsc::channel();
    let observer=std::thread::spawn(move|| {let _=done.send(joining.join_remaining());});
    assert!(wait_done.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(count.load(Ordering::Acquire),1);assert!(connections.lock().unwrap().contains_key(&7));
    peer.set_read_timeout(Some(Duration::from_millis(20))).unwrap();let mut byte=[0];
    assert!(matches!(peer.read(&mut byte),Err(ref error) if matches!(error.kind(),io::ErrorKind::WouldBlock|io::ErrorKind::TimedOut)));
    release.open();assert!(wait_done.recv_timeout(Duration::from_secs(2)).unwrap());observer.join().unwrap();
    assert_eq!(count.load(Ordering::Acquire),0);assert!(connections.lock().unwrap().is_empty());
    assert_eq!(peer.read(&mut byte).unwrap(),0,"last retained socket closes only after actual original TLS join");
}

#[test]
fn cancel_admission_ordinary_and_control_connects_keep_original_saturated_backlog_budgets() {
    for control in [false,true] {
        let f=Fixture::new();let (listener,held)=saturated_unaccepted_backlog(&f.socket);
        let client=BrokerClient {socket:f.socket.clone(),identity:f.socket.with_file_name("identity.json"),binding:BrokerBinding::from(&f.a)};
        let (send,wait)=mpsc::channel();let offered=f.a.clone();let start=Instant::now();
        let worker=std::thread::spawn(move|| {
            if control {client.cancel(&uuid::Uuid::new_v4().to_string(),Instant::now()+Duration::from_millis(80));}
            else {assert!(client.exchange_once(Operation::Call {tool:"agents_inbox".into(),arguments:json!({})},offered,
                &AtomicBool::new(false),Instant::now()+Duration::from_millis(80)).is_err());}
            let _=send.send(());
        });
        let bounded=wait.recv_timeout(Duration::from_millis(500));
        // Always unwind a legacy blocking-connect control before failing.
        drop(held);drop(listener);worker.join().unwrap();
        assert!(bounded.is_ok(),"full backlog bypassed original ordinary/control connect deadline");
        assert!(start.elapsed()<Duration::from_secs(1));
    }
}
