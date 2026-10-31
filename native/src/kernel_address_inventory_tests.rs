use super::*;
use std::{os::fd::AsFd, sync::{mpsc, Condvar}, thread};

fn message(kind: u16, flags: u16, sequence: u32, header_pid: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::from(((HEADER + payload.len()) as u32).to_ne_bytes());
    bytes.extend(kind.to_ne_bytes()); bytes.extend(flags.to_ne_bytes());
    bytes.extend(sequence.to_ne_bytes()); bytes.extend(header_pid.to_ne_bytes()); bytes.extend(payload);
    while bytes.len() % 4 != 0 { bytes.push(0); } bytes
}
fn attribute(kind: u16, value: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::from(((4 + value.len()) as u16).to_ne_bytes());
    bytes.extend(kind.to_ne_bytes()); bytes.extend(value);
    while bytes.len() % 4 != 0 { bytes.push(0); } bytes
}
fn ipv4(value: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![libc::AF_INET as u8, 8, 0, 0]; bytes.extend(1u32.to_ne_bytes());
    bytes.extend(attribute(2, &value)); bytes
}
fn packet(bytes: Vec<u8>) -> Packet { Packet { bytes, sender: 0 } }

#[test]
fn actual_kernel_dump_and_descriptors_remain_owned_by_the_snapshot() {
    let owner = KernelAddressCustody::capture_current().unwrap();
    assert_eq!(owner.process_id(), std::process::id());
    assert_eq!(owner.namespace_identity(), current_net().unwrap().identity());
    assert_eq!(owner.namespace_identity().kind(), NamespaceKind::Network);
    assert_eq!(owner.boot_id().len(), 36); assert_ne!(owner.netns_cookie(), 0);
    for fd in [owner.pidfd.as_raw_fd(), owner.probe.as_raw_fd(), owner.monitor.fd.as_raw_fd(),
        owner.namespace.as_fd().as_raw_fd(), owner.boot.file.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    }
    let weak = Arc::downgrade(&owner);
    let snapshot = owner.snapshot(Duration::from_millis(200)).unwrap();
    assert!(snapshot.addresses().unwrap().contains(&Ipv4Addr::LOCALHOST));
    assert!(snapshot.addresses().unwrap().len() <= MAX_HOST_IPV4_ADDRESSES);
    assert_eq!(snapshot.revision(), 1);
    drop(owner); assert!(weak.upgrade().is_some()); snapshot.check_current().unwrap();
    drop(snapshot); assert!(weak.upgrade().is_none());
}

#[test]
fn genuine_fresh_dump_does_not_renew_an_older_snapshot_cutoff() {
    let owner = KernelAddressCustody::capture_current().unwrap();
    let old = owner.snapshot(Duration::from_millis(100)).unwrap(); let end = old.original_deadline();
    thread::sleep(Duration::from_millis(125));
    assert_eq!(old.check_current(), Err(KernelInventoryError::Deadline));
    assert!(!owner.fenced.load(Ordering::Acquire));
    let fresh = owner.snapshot(Duration::from_millis(200)).unwrap(); fresh.check_current().unwrap();
    assert!(fresh.original_deadline().boottime_ns() > end.boottime_ns());
    assert_eq!(old.original_deadline().wall_ms(), end.wall_ms());
    assert_eq!(old.original_deadline().boottime_ns(), end.boottime_ns());
    assert_eq!(old.addresses().err(), Some(KernelInventoryError::Deadline));
}

#[test]
fn actual_descriptor_boot_process_cookie_and_clock_faults_are_absorbing() {
    for fault in ["process", "namespace", "boot", "cookie", "clock"] {
        let mut owner = KernelAddressCustody::capture_current().unwrap();
        let value = Arc::get_mut(&mut owner).unwrap();
        match fault {
            "process" => value.process = value.process.checked_add(1).unwrap(),
            "namespace" => {
                let user = File::open("/proc/thread-self/ns/user").unwrap();
                value.namespace = NamespaceFd::inspect(user.into(), NamespaceKind::User).unwrap();
            }
            "boot" => value.boot.bytes[0] ^= 1,
            "cookie" => value.net_cookie ^= 1,
            _ => value.state.lock().unwrap().last.boot_ns = u128::MAX,
        }
        assert!(owner.snapshot(Duration::from_millis(200)).is_err(), "{fault}");
        assert!(owner.fenced.load(Ordering::Acquire), "{fault}");
        assert_eq!(owner.snapshot(Duration::from_millis(200)).err(), Some(KernelInventoryError::Fenced));
    }
}

#[test]
fn queued_changes_and_overflow_invalidate_revision_without_synthetic_kernel_mutation() {
    for kind in [RTM_NEWADDR, NLMSG_OVERRUN] {
        let owner = KernelAddressCustody::capture_current().unwrap();
        let snapshot = owner.snapshot(Duration::from_millis(200)).unwrap();
        let mut state = owner.state.lock().unwrap();
        let event = packet(message(kind, 0, 0, 0, &ipv4([127, 0, 0, 1])));
        assert!(owner.observe_change(&mut state, event).is_err());
        assert!(state.revision > snapshot.revision()); drop(state);
        assert_eq!(snapshot.check_current(), Err(KernelInventoryError::Fenced));
        assert!(owner.snapshot(Duration::from_millis(200)).is_err());
    }
    // The synthetic frames above can only invalidate an ACTUAL retained owner;
    // no parser/byte/address constructor can create that owner or a snapshot.
}

struct Gate { entered: mpsc::SyncSender<()>, released: Mutex<bool>, wake: Condvar }
impl Gate {
    fn release(&self) { *self.released.lock().unwrap() = true; self.wake.notify_all(); }
    fn wait(&self) -> Result<()> {
        self.entered.send(()).map_err(|_| KernelInventoryError::Io)?;
        let mut released = self.released.lock().unwrap();
        while !*released { released = self.wake.wait(released).unwrap(); }
        Ok(())
    }
}
struct Worker { gate: Arc<Gate>, handle: Option<thread::JoinHandle<Result<KernelIpv4Snapshot>>> }
impl Worker {
    fn join(mut self) -> thread::Result<Result<KernelIpv4Snapshot>> {
        self.gate.release(); self.handle.take().unwrap().join()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.gate.release(); if let Some(handle) = self.handle.take() { let _ = handle.join(); }
    }
}

#[test]
fn concurrent_absorbing_fence_cannot_be_overridden_by_earlier_success() {
    for operation in ["addresses", "snapshot"] {
        let owner = KernelAddressCustody::capture_current().unwrap();
        let (entered, receive) = mpsc::sync_channel(1);
        let gate = Arc::new(Gate { entered, released: Mutex::new(false), wake: Condvar::new() });
        let retained = owner.clone(); let blocked = gate.clone();
        let worker = Worker { gate, handle: Some(thread::spawn(move || {
            if operation == "addresses" {
                let snapshot = retained.snapshot(Duration::from_millis(200))?;
                // Pause after the initial complete current check and before
                // exposing addresses. No later helper check masks finish().
                snapshot.addresses_inner(|| blocked.wait())?;
                Ok(snapshot)
            } else {
                // Pause after the initial fence check and real dump while the
                // state mutex is held. Invalid lifetime validation fences
                // outside that mutex, exactly like the reported race.
                retained.snapshot_inner(Duration::from_millis(200), || blocked.wait())
            }
        })) };
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!owner.fenced.load(Ordering::Acquire));
        assert!(matches!(owner.snapshot(Duration::ZERO), Err(KernelInventoryError::Limit)));
        assert!(owner.fenced.load(Ordering::Acquire));
        assert!(matches!(worker.join().unwrap(), Err(KernelInventoryError::Fenced)), "{operation}");
        assert!(matches!(owner.snapshot(Duration::from_millis(200)), Err(KernelInventoryError::Fenced)));
    }
}

#[test]
fn late_real_dump_keeps_actual_custody_until_callback_return_and_join() {
    let owner = KernelAddressCustody::capture_current().unwrap(); let weak = Arc::downgrade(&owner);
    let (entered, receive) = mpsc::sync_channel(1);
    let gate = Arc::new(Gate { entered, released: Mutex::new(false), wake: Condvar::new() });
    let retained = owner.clone(); let blocked = gate.clone();
    let worker = Worker { gate, handle: Some(thread::spawn(move ||
        retained.snapshot_inner(Duration::from_millis(200), || blocked.wait()))) };
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    thread::sleep(Duration::from_millis(225)); drop(owner);
    assert!(!worker.handle.as_ref().unwrap().is_finished()); assert!(weak.upgrade().is_some());
    assert!(matches!(worker.join().unwrap(), Err(KernelInventoryError::Deadline)));
    assert!(weak.upgrade().is_none(), "only actual callback/worker settlement releases custody");
}

#[test]
fn multipart_dump_uses_sockaddr_sender_not_the_header_pid_as_kernel_identity() {
    let mut bytes = message(RTM_NEWADDR, NLM_F_MULTI, 7, 12345, &ipv4([127, 0, 0, 1]));
    bytes.extend(message(NLMSG_DONE, NLM_F_MULTI, 7, 12345, &0i32.to_ne_bytes()));
    let mut addresses = BTreeSet::new(); let mut count = 0;
    assert!(consume_dump(&packet(bytes.clone()), 7, &mut addresses, &mut count).unwrap());
    assert!(addresses.contains(&Ipv4Addr::LOCALHOST));
    let forged = Packet { bytes, sender: 54321 };
    assert_eq!(consume_dump(&forged, 7, &mut BTreeSet::new(), &mut 0), Err(KernelInventoryError::Incomplete));
}

#[test]
fn actual_kernel_datagram_truncation_cannot_be_accepted_as_a_partial_dump() {
    let owner = KernelAddressCustody::capture_current().unwrap();
    let route = RouteSocket::bind(0, owner.netns_cookie()).unwrap(); route.request_dump(1).unwrap();
    let mut poll = libc::pollfd { fd: route.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1);
    assert_ne!(poll.revents & libc::POLLIN, 0);
    // The tiny receive capacity is private fault injection on this actual held
    // kernel socket, not a constructor from synthetic inventory/packet DATA.
    assert!(matches!(receive_limit(route.fd.as_raw_fd(), 1), Err(KernelInventoryError::Incomplete)));
}

#[test]
fn interrupted_truncated_unfinished_unknown_and_error_dumps_never_complete() {
    let data = message(RTM_NEWADDR, NLM_F_MULTI, 7, 0, &ipv4([127, 0, 0, 1]));
    assert!(!consume_dump(&packet(data.clone()), 7, &mut BTreeSet::new(), &mut 0).unwrap(),
        "address records without DONE are only partial DATA");
    let mut cases = vec![data[..data.len()-1].to_vec(),
        message(RTM_NEWADDR, NLM_F_MULTI | NLM_F_DUMP_INTR, 7, 0, &ipv4([127, 0, 0, 1])),
        message(RTM_NEWADDR, NLM_F_MULTI, 8, 0, &ipv4([127, 0, 0, 1])),
        message(2, NLM_F_MULTI, 7, 0, &(-libc::ENOBUFS).to_ne_bytes()),
        message(NLMSG_OVERRUN, NLM_F_MULTI, 7, 0, &[]),
        message(NLMSG_DONE, NLM_F_MULTI, 7, 0, &0i32.to_ne_bytes())];
    let mut unknown = ipv4([127, 0, 0, 1]); unknown.extend(attribute(0x3fff, &[0; 4]));
    cases.push(message(RTM_NEWADDR, NLM_F_MULTI, 7, 0, &unknown));
    let mut wrong_family = ipv4([127, 0, 0, 1]); wrong_family[0] = libc::AF_INET6 as u8;
    cases.push(message(RTM_NEWADDR, NLM_F_MULTI, 7, 0, &wrong_family));
    let mut trailing = data; trailing.extend(message(NLMSG_DONE, NLM_F_MULTI, 7, 0, &0i32.to_ne_bytes()));
    trailing.extend(message(RTM_NEWADDR, NLM_F_MULTI, 7, 0, &ipv4([127, 0, 0, 2]))); cases.push(trailing);
    for bytes in cases {
        assert!(consume_dump(&packet(bytes), 7, &mut BTreeSet::new(), &mut 0).is_err());
    }
}

#[test]
fn peer_and_local_addresses_are_both_conservatively_included_with_fixed_limits() {
    let mut payload = ipv4([127, 0, 0, 1]); payload.extend(attribute(1, &[198, 51, 100, 7]));
    let mut addresses = BTreeSet::new(); address(&payload, &mut addresses).unwrap();
    assert!(addresses.contains(&Ipv4Addr::LOCALHOST)); assert!(addresses.contains(&Ipv4Addr::new(198, 51, 100, 7)));
    for i in 0..=MAX_HOST_IPV4_ADDRESSES {
        let value = (0x0b00_0000u32 + i as u32).to_be_bytes();
        if let Err(error) = address(&ipv4(value), &mut addresses) {
            assert_eq!(error, KernelInventoryError::Limit); return;
        }
    }
    panic!("an oversized kernel address set must not become a usable snapshot");
}
