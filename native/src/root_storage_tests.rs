//! Written, UNRUN kernel transport/codec/cancellation tests. These do not claim
//! root bootstrap, a mounted seed, helper drain or production activation passed.
use super::*;
use std::time::Instant;

fn pair() -> (Endpoint, Endpoint) {
    let mut raw = [-1; 2];
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX,
        libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0, raw.as_mut_ptr()) }, 0);
    (Endpoint::capture(unsafe { OwnedFd::from_raw_fd(raw[0]) }).unwrap(),
     Endpoint::capture(unsafe { OwnedFd::from_raw_fd(raw[1]) }).unwrap())
}
fn receive(endpoint: &Endpoint) -> Packet {
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(packet) = endpoint.receive().unwrap() { return packet; }
        assert!(Instant::now() < until); std::thread::sleep(Duration::from_millis(1));
    }
}
fn budget() -> FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(2)).unwrap() }

#[test]
fn actual_anonymous_seqpacket_delivers_kernel_credentials_and_cloexec_eventfd() {
    let (sender, reader) = pair(); let cancel = eventfd().unwrap();
    assert!(sender.send(b"bound original frame", Some(&cancel)).unwrap());
    let mut packet = receive(&reader);
    assert_eq!(packet.bytes, b"bound original frame");
    assert_eq!(packet.credentials.pid, std::process::id() as i32);
    assert_eq!(packet.credentials.uid, unsafe { libc::getuid() });
    assert_eq!(packet.credentials.gid, unsafe { libc::getgid() });
    let actual = packet.fds.pop().unwrap(); assert!(packet.fds.is_empty());
    assert_ne!(unsafe { libc::fcntl(actual.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
    cancel_event(&cancel);
    let mut count = 0u64;
    assert_eq!(unsafe { libc::read(actual.as_raw_fd(), (&mut count as *mut u64).cast(), 8) }, 8);
    assert_eq!(count, 1);
}

#[test]
fn stream_and_regular_file_cannot_be_adopted_as_storage_bootstrap_endpoint() {
    let (stream, _) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(Endpoint::capture(stream.into()).is_err());
    assert!(Endpoint::capture(File::open("/dev/null").unwrap().into()).is_err());
}

#[test]
fn truncated_seqpacket_is_rejected_instead_of_parsing_a_valid_prefix() {
    let (sender, reader) = pair(); let bytes = vec![b'x'; MAX_PACKET + 1];
    assert_eq!(unsafe { libc::send(sender.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL) }, bytes.len() as isize);
    assert!(reader.receive().is_err());
}

#[test]
fn original_constraint_serial_and_binary_chunk_round_trip_without_path_fields() {
    let original = budget(); let slot = uuid::Uuid::new_v4();
    let frame = Envelope { version: 1, serial: 11, constraint: original.constraint(), request: Request::Chunk { slot, offset: 32768 } };
    let payload = [0u8, 255, b'\n', b'\0', b'x']; let bytes = encode(&frame, &payload).unwrap();
    let (decoded, data): (Envelope, _) = decode(&bytes).unwrap();
    assert_eq!(decoded.version, 1); assert_eq!(decoded.serial, 11); assert_eq!(decoded.constraint, original.constraint());
    assert!(matches!(decoded.request, Request::Chunk { slot: value, offset: 32768 } if value == slot));
    assert_eq!(data, payload);
    assert!(encode(&frame, &vec![0; CHUNK + 1]).is_err());
    assert!(decode::<Envelope>(&[255, 255, 255, 255, b'{']).is_err());
}

#[test]
fn public_drain_boolean_and_user_path_operations_have_no_wire_decoder() {
    let constraint = serde_json::to_value(budget().constraint()).unwrap();
    for request in [serde_json::json!({"op":"retire","slot":uuid::Uuid::new_v4().to_string(),"drained":true}),
        serde_json::json!({"op":"mount","root":"/workspace/user-selected"}),
        serde_json::json!({"op":"hello","constraint":constraint.clone(),"user_path":"/workspace"})] {
        let wire = serde_json::json!({"version":1,"serial":1,"constraint":constraint,"request":request});
        let bytes = encode(&wire, &[]).unwrap(); assert!(decode::<Envelope>(&bytes).is_err());
    }
}

#[test]
fn refs_bytes_and_typed_object_limits_are_checked_before_buffer_allocation() {
    let seed = ObjectRef::from_bytes(ObjectKind::Directory, b"small-root");
    assert!(admit(&seed, 1, seed.size_bytes).is_ok());
    assert!(admit(&seed, MAX_REFS + 1, 1).is_err());
    assert!(admit(&seed, 1, MAX_TRANSFER_BYTES + 1).is_err());
    assert!(admit(&ObjectRef::from_bytes(ObjectKind::Blob, b"not-root"), 1, 8).is_err());
    for (kind, bound) in [(ObjectKind::Directory, 256 * 1024), (ObjectKind::DirectoryShard, 64 * 1024),
        (ObjectKind::File, 64 * 1024), (ObjectKind::Blob, MAX_OBJECT_BYTES)] {
        let mut reference = ObjectRef::from_bytes(kind, b"bounded"); reference.size_bytes = bound;
        assert!(object_bound(&reference, MAX_TRANSFER_BYTES).is_ok());
        reference.size_bytes += 1; assert!(object_bound(&reference, MAX_TRANSFER_BYTES).is_err());
    }
    assert!(object_bound(&ObjectRef::from_bytes(ObjectKind::EnvironmentRoot, b"not-seed-closure"), MAX_TRANSFER_BYTES).is_err());
}

#[test]
fn source_inventory_stops_at_unique_ref_bound_before_reading_any_oversized_child() {
    let root = ObjectRef::from_bytes(ObjectKind::Directory, b"root"); let mut reads = 0usize;
    let result = crate::root_storage_client::bounded_inventory(&root, &budget(), |_| {
        reads += 1;
        Ok((0..MAX_REFS).map(|i| ObjectRef::from_bytes(ObjectKind::Blob, &i.to_be_bytes())).collect())
    });
    assert!(result.is_err()); assert_eq!(reads, 1);
    let mut too_big = ObjectRef::from_bytes(ObjectKind::Blob, b"too-big"); too_big.size_bytes = MAX_OBJECT_BYTES + 1;
    let mut reads = 0usize;
    assert!(crate::root_storage_client::bounded_inventory(&root, &budget(), |_| { reads += 1; Ok(vec![too_big.clone()]) }).is_err());
    assert_eq!(reads, 1);
}

#[test]
fn actual_eventfd_cancellation_retires_the_same_service_budget_before_observer_join() {
    let original = budget(); let source = eventfd().unwrap();
    let mut observer = Cancellation::capture(source.try_clone().unwrap()).unwrap();
    observer.start(original.clone()).unwrap(); cancel_event(&source);
    let until = Instant::now() + Duration::from_secs(1);
    while !observer.flag.load(Ordering::Acquire) && Instant::now() < until { std::thread::sleep(Duration::from_millis(1)); }
    assert!(observer.flag.load(Ordering::Acquire)); assert!(original.remaining().is_err());
    observer.finish().unwrap();
    assert!(observer.worker.is_none() && observer.result.as_ref().unwrap().is_ok());
}

#[test]
fn cancellation_descriptor_rejects_an_arbitrary_readable_file() {
    assert!(Cancellation::capture(File::open("/dev/null").unwrap().into()).is_err());
}

#[test]
fn process_custody_uses_an_actual_live_pidfd_not_a_saved_pid_or_regular_file() {
    let current = Process::current().unwrap(); assert_eq!(current.pid, std::process::id());
    assert!(!current.exited().unwrap()); current.live().unwrap();
    assert!(Process::capture(File::open("/dev/null").unwrap().into()).is_err());
}

#[test]
fn one_helper_exit_never_replaces_all_sibling_process_exits_or_drops_their_pidfds() {
    use std::process::{Child, Command, Stdio};
    struct Children(Vec<Child>);
    impl Drop for Children { fn drop(&mut self) { for child in &mut self.0 { let _ = child.kill(); let _ = child.wait(); } } }
    let mut children = Children(Vec::new()); let mut helpers = BTreeMap::new();
    for _ in 0..2 {
        children.0.push(Command::new("/usr/bin/sleep").arg("30").env_clear()
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, children.0.last().unwrap().id(), 0) } as i32;
        assert!(raw >= 0);
        helpers.insert(uuid::Uuid::new_v4(), Process::capture(unsafe { OwnedFd::from_raw_fd(raw) }).unwrap());
    }
    assert_eq!(helpers.len(), 2); assert!(!all_helpers_exited(&helpers).unwrap());
    children.0[0].kill().unwrap(); children.0[0].wait().unwrap();
    assert_eq!(helpers.len(), 2); assert!(!all_helpers_exited(&helpers).unwrap());
    assert_eq!(helpers.values().filter(|helper| helper.exited().unwrap()).count(), 1);
    children.0[1].kill().unwrap(); children.0[1].wait().unwrap();
    assert_eq!(helpers.len(), 2); assert!(all_helpers_exited(&helpers).unwrap());
    assert!(all_helpers_exited(&BTreeMap::new()).is_err());
}
