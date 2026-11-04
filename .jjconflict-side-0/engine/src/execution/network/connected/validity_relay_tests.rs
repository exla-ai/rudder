// Included in relay::tests; all authorities here are synthetic retained test
// owners, never production inventory/grant constructors.
struct Release(Option<mpsc::SyncSender<()>>);
impl Release { fn open(&mut self) { self.0.take().unwrap().send(()).unwrap(); } }
impl Drop for Release { fn drop(&mut self) { if let Some(send) = self.0.take() { let _ = send.try_send(()); } } }

struct DelayedAuthority {
    grant:ConnectDeadline, inventory:ConnectDeadline, producer:ConnectDeadline,
    entered:mpsc::SyncSender<()>, release:Mutex<mpsc::Receiver<()>>,
}
impl ConnectValidationGuard for DelayedAuthority {
    fn check_current(&self) -> Result<()> { Ok(()) }
}
impl ConnectAuthority for DelayedAuthority {
    fn validate(self:Arc<Self>, query:ConnectQuery) -> Result<ConnectValidation> {
        let host = HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST]).unwrap();
        let validation = ConnectValidation::new(query, host, self.grant, self.inventory, self.producer, self.clone())?;
        self.entered.send(()).map_err(|_|Error::authority_denied())?;
        self.release.lock().unwrap().recv_timeout(Duration::from_secs(2)).map_err(|_|Error::authority_denied())?;
        Ok(validation) // Was valid before blocking. Runtime must recheck on return.
    }
}

#[test]
fn delayed_callback_cannot_adopt_after_original_query_grant_inventory_or_producer_expiry() {
    // 0: engine adoption; 1: original grant; 2: inventory; 3: producer query.
    for expired in 0..4 {
        let mut fixture = Fixture::new();
        let (entered, wait) = mpsc::sync_channel(1); let (release, gate) = mpsc::sync_channel(1);
        let mut release = Release(Some(release));
        let fixed = qualifier_deadline(60_000); let shorter = qualifier_deadline(100);
        let authority = Arc::new(DelayedAuthority { grant:if expired == 1 { shorter } else { fixed },
            inventory:if expired == 2 { shorter } else { fixed }, producer:if expired == 3 { shorter } else { fixed },
            entered, release:Mutex::new(gate) });
        Arc::get_mut(&mut fixture.shared).unwrap().authority = authority.clone();
        let previous = fixture.shared.validation().unwrap();
        let shared = fixture.shared.clone(); let (returned, result) = mpsc::sync_channel(1);
        let mut worker = Worker::spawn("connect-delayed-valid-result", move || {
            let answer = shared.grant().and_then(|validation|shared.adopt_grant(validation));
            returned.send(answer).unwrap();
        }).unwrap();
        wait.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(worker.settle(Instant::now() + Duration::from_millis(10)).unwrap_err().kind(), Kind::Unsettled);
        assert!(worker.join.is_some() && Arc::strong_count(&authority) >= 3);
        thread::sleep(Duration::from_millis(if expired == 0 { 225 } else { 125 }));
        release.open();
        assert_eq!(result.recv_timeout(Duration::from_secs(1)).unwrap().unwrap_err().kind(), Kind::AuthorityDenied);
        worker.settle(Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(worker.join.is_none() && fixture.shared.cancelled());
        assert!(Arc::ptr_eq(&previous, &fixture.shared.validation().unwrap()));
        assert_eq!(fixture.shared.stats.upstreams.load(Ordering::Acquire), 0);
    }
}

#[test]
fn returning_a_different_original_query_cannot_substitute_matching_metadata() {
    struct Substitute(Mutex<Option<ConnectQuery>>, ConnectDeadline);
    impl ConnectValidationGuard for Substitute { fn check_current(&self) -> Result<()> { Ok(()) } }
    impl ConnectAuthority for Substitute {
        fn validate(self:Arc<Self>, _actual:ConnectQuery) -> Result<ConnectValidation> {
            let other = self.0.lock().unwrap().take().unwrap();
            let host = HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST]).unwrap();
            ConnectValidation::new(other, host, self.1, self.1, self.1, self)
        }
    }
    let mut fixture = Fixture::new(); let previous = fixture.shared.validation().unwrap();
    let other = ConnectQuery::begin(&fixture.shared.binding).unwrap();
    Arc::get_mut(&mut fixture.shared).unwrap().authority = Arc::new(Substitute(Mutex::new(Some(other)), qualifier_deadline(60_000)));
    assert_eq!(fixture.shared.grant().unwrap_err().kind(), Kind::BindingMismatch);
    assert!(fixture.shared.cancelled());
    assert!(Arc::ptr_eq(&previous, &fixture.shared.validation().unwrap()));
}

struct BlockingGuard {
    block:AtomicBool, entered:mpsc::SyncSender<()>, release:Mutex<mpsc::Receiver<()>>,
}
impl ConnectValidationGuard for BlockingGuard {
    fn check_current(&self) -> Result<()> {
        if self.block.swap(false, Ordering::AcqRel) {
            self.entered.send(()).map_err(|_|Error::authority_denied())?;
            self.release.lock().unwrap().recv_timeout(Duration::from_secs(2)).map_err(|_|Error::authority_denied())?;
        }
        Ok(())
    }
}

#[test]
fn callback_free_watchdog_closes_sockets_while_effect_guard_stalls_and_keeps_actual_worker_owned() {
    let fixture = Fixture::new();
    let (entered, wait) = mpsc::sync_channel(1); let (release, gate) = mpsc::sync_channel(1);
    let mut release = Release(Some(release));
    let guard = Arc::new(BlockingGuard { block:AtomicBool::new(false), entered, release:Mutex::new(gate) });
    let query = ConnectQuery::begin(&fixture.shared.binding).unwrap();
    let host = HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST]).unwrap();
    let validation = Arc::new(ConnectValidation::new(query, host, qualifier_deadline(60_000), qualifier_deadline(150), qualifier_deadline(60_000), guard.clone()).unwrap());
    fixture.shared.adopt_grant(validation.clone()).unwrap();
    let (mut peer, proxy) = pair();
    fixture.shared.stats.active.store(1, Ordering::Release);
    let connection = Connection { stream:TrackedStream::new(proxy, &fixture.shared),
        _permit:ActivePermit(fixture.shared.clone()), accepted:Instant::now() };
    let shared = fixture.shared.clone();
    guard.block.store(true, Ordering::Release);
    let worker = Worker::spawn("connect-blocked-effect-guard", move || {
        let mut connection = connection;
        let mut meter = Meter { shared:&shared, used:0 };
        assert!(meter.read(&mut connection.stream.stream, &mut [0u8;1]).is_err());
    }).unwrap();
    let mut owner = OwnedConnectProxy { shared:fixture.shared.clone(), workers:vec![worker], ready:None, settled:false };
    wait.recv_timeout(Duration::from_secs(1)).unwrap();
    // These actual IO ownership locks must remain free during the arbitrary
    // callback, independently of its worker's continued custody/charge.
    assert!(fixture.shared.validation.try_lock().is_ok());
    assert!(fixture.shared.sockets.try_lock().is_ok());
    let end = Instant::now() + Duration::from_secs(1);
    while validation.check_bounds().is_ok() {
        assert!(Instant::now() < end); thread::sleep(Duration::from_millis(1));
    }
    assert!(!fixture.shared.keep_admitted());
    assert_eq!(peer.read(&mut [0u8;1]).unwrap(), 0); // Independent real socket shutdown.
    assert!(matches!(owner.close_and_settle(Duration::from_millis(10)), Err(error) if error.kind() == Kind::Unsettled));
    assert_eq!(fixture.shared.stats.active.load(Ordering::Acquire), 1);
    assert!(fixture.shared.witnesses.lock().unwrap().is_some());
    assert!(owner.workers[0].join.is_some());
    release.open();
    let settled = owner.close_and_settle(Duration::from_secs(1)).unwrap();
    assert_eq!(settled.snapshot().active_connections, 0);
    assert!(fixture.shared.sockets.lock().unwrap().is_empty());
    assert!(fixture.shared.witnesses.lock().unwrap().is_none());
}
