//! Real transport/FD negative mechanisms. No synthetic successful Root ready
//! ACK, client, registered owner, mount, helper or retirement is manufactured.
use super::*;

fn pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX,
        libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}
fn deadline(constraint: &FinalizationConstraint) -> u128 {
    serde_json::to_value(constraint).unwrap()["boottime_deadline_nanoseconds"].as_str().unwrap().parse().unwrap()
}

#[test]
fn late_reordered_or_replaced_original_cutoff_refuses_without_new_budget() {
    let original = FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let constraint = original.constraint(); let end = deadline(&constraint);
    assert!(original_receipt_window(&constraint, &(end - 1).to_string(), &end.to_string()).is_err());
    assert!(original_receipt_window(&constraint, &end.to_string(), &end.to_string()).is_err());
    assert!(original_receipt_window(&constraint, &(end - 1).to_string(), &(end - 2).to_string()).is_err());
    assert!(original_receipt_window(&constraint, "00", &(end - 1).to_string()).is_err());
    let changed = FinalizationBudget::begin(Duration::from_millis(5)).unwrap().constraint();
    assert!(original_receipt_window(&changed, "1", "2").is_err());
    assert_eq!(original.constraint(), constraint, "time validation created/replaced no original clock");
}

#[test]
fn original_gate_rejects_foreign_credentials_and_retains_received_actual_fd() {
    let (left, right) = pair(); let sender = Endpoint::capture(left).unwrap();
    let original = FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let data = BootstrapData { version: 1, nonce: uuid::Uuid::new_v4().to_string(), constraint: original.constraint() };
    let own = Process::current().unwrap();
    // Deliberately foreign UID even when this normal test runs as root. It is
    // an actual SCM-credentials refusal, not a substitute Root bootstrap.
    let expected_uid = unsafe { libc::geteuid() } ^ 1;
    let mut receiver = Gate::capture(right, own.fd().try_clone().unwrap(), expected_uid, data.clone(), original.clone()).unwrap();
    let frame = Frame { version: 1, serial: 1, nonce: data.nonce, constraint: data.constraint,
        message: Message::RuntimeImageReadyEvidence };
    let file: OwnedFd = File::open("/dev/null").unwrap().into();
    let bytes = storage::encode(&frame, &[]).unwrap();
    assert!(sender.send(&bytes, Some(&file)).unwrap());
    assert!(receiver.poll().is_err());
    let packet = receiver.pending_packet.as_ref().expect("rejected original packet stays retained");
    assert_eq!(packet.fds.len(), 1); assert_eq!(packet.credentials.uid, unsafe { libc::geteuid() });
    assert!(unsafe { libc::fcntl(packet.fds[0].as_raw_fd(), libc::F_GETFD) } >= 0);
    assert!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } >= 0);
    original.cancel(); assert!(receiver.poll().is_err());
    assert_eq!(receiver.pending_packet.as_ref().unwrap().fds.len(), 1);
}

#[test]
fn actual_untrusted_seqpacket_and_fd_do_not_decode_as_ready_evidence() {
    let (left, right) = pair();
    let sender = RuntimeEndpoint::capture(left).unwrap(); let receiver = RuntimeEndpoint::capture(right).unwrap();
    let original: OwnedFd = File::open("/dev/null").unwrap().into();
    assert!(sender.send_data(br#"{"ready":true,"mount_id":7}"#, &[&original]).unwrap());
    let packet = receiver.receive_data().unwrap().unwrap();
    assert_eq!(packet.credentials.pid, std::process::id() as i32); assert_eq!(packet.fds.len(), 1);
    assert!(serde_json::from_slice::<ReadyEvidenceData>(&packet.bytes).is_err());
    assert!(super::super::super::read_sealed_bytes(&packet.fds[0], READY_LIMIT).is_err());
    assert!(unsafe { libc::fcntl(packet.fds[0].as_raw_fd(), libc::F_GETFD) } >= 0);
    assert!(unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFD) } >= 0);
}

#[test]
fn stale_serial_or_foreign_bootstrap_nonce_retains_packet_without_observation() {
    for foreign_nonce in [false, true] {
        let (left, right) = pair(); let sender = Endpoint::capture(left).unwrap();
        let original = FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
        let data = BootstrapData { version: 1, nonce: uuid::Uuid::new_v4().to_string(), constraint: original.constraint() };
        let own = Process::current().unwrap();
        let mut receiver = Gate::capture(right, own.fd().try_clone().unwrap(), unsafe { libc::geteuid() }, data.clone(), original).unwrap();
        let frame = Frame { version: 1, serial: if foreign_nonce { 1 } else { 2 },
            nonce: if foreign_nonce { uuid::Uuid::new_v4().to_string() } else { data.nonce },
            constraint: data.constraint, message: Message::RuntimeImageReadyEvidence };
        let file: OwnedFd = File::open("/dev/null").unwrap().into();
        assert!(sender.send(&storage::encode(&frame, &[]).unwrap(), Some(&file)).unwrap());
        assert!(receiver.poll().is_err());
        assert_eq!(receiver.received, 0);
        assert_eq!(receiver.pending_packet.as_ref().unwrap().fds.len(), 1);
        assert!(unsafe { libc::fcntl(receiver.pending_packet.as_ref().unwrap().fds[0].as_raw_fd(), libc::F_GETFD) } >= 0);
    }
}

#[test]
fn pending_bootstrap_cannot_issue_observation_and_keeps_partial_ready_fd() {
    let mut bootstrap = ManagedMachineBootstrap::empty();
    let original: OwnedFd = File::open("/dev/null").unwrap().into();
    let n = original.as_raw_fd(); bootstrap.runtime_ready_fd = Some(original);
    assert!(bootstrap.adopt_runtime_image_retaining().is_err());
    assert!(bootstrap.runtime_client.is_none() && bootstrap.runtime_ready_observation.is_none());
    assert_eq!(bootstrap.runtime_ready_fd.as_ref().unwrap().as_raw_fd(), n);
    assert!(unsafe { libc::fcntl(n, libc::F_GETFD) } >= 0);
}

#[test]
fn endpoint_without_original_ready_evidence_never_becomes_an_observation() {
    let mut bootstrap = ManagedMachineBootstrap::empty(); let (one, two) = pair();
    let n = one.as_raw_fd(); bootstrap.runtime_fd = Some(one);
    assert!(bootstrap.adopt_runtime_image_retaining().is_err());
    assert!(bootstrap.runtime_ready_observation.is_none() && bootstrap.runtime_ready_fd.is_none());
    assert_eq!(bootstrap.runtime_fd.as_ref().unwrap().as_raw_fd(), n);
    assert!(unsafe { libc::fcntl(n, libc::F_GETFD) } >= 0); drop(two);
}

#[test]
fn malformed_ready_memfd_read_failure_keeps_callers_actual_owner() {
    let original = super::super::super::sealed_bytes(b"not ready JSON").unwrap();
    let n = original.as_raw_fd();
    // Root metadata is required in production. Ordinary UID1500 tests may stop
    // at that check; root tests still fail JSON decoding. Neither issues owner.
    if let Ok(bytes) = super::super::super::read_sealed_bytes(&original, READY_LIMIT) {
        assert!(serde_json::from_slice::<ReadyEvidenceData>(&bytes).is_err());
    }
    assert!(unsafe { libc::fcntl(n, libc::F_GETFD) } >= 0);
    assert!(unsafe { libc::fcntl(n, libc::F_GET_SEALS) } & SEALS == SEALS);
}
