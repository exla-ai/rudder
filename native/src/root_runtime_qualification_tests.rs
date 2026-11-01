//! Mechanism/refusal tests only. No successful Root ACK, observation, engine,
//! scoped provider custody or published qualification owner is manufactured.
use super::*;

#[test]
fn actual_witness_is_readonly_sealed_cloexec_and_cannot_be_rebuilt() {
    let mut witness = WitnessFile::staged(); witness.build(b"actual bounded test DATA").unwrap();
    let read = witness.fd().unwrap();
    assert_ne!(unsafe {libc::fcntl(read.as_raw_fd(),libc::F_GETFD)} & libc::FD_CLOEXEC, 0);
    assert_eq!(unsafe {libc::fcntl(read.as_raw_fd(),libc::F_GETFL)} & libc::O_ACCMODE, libc::O_RDONLY);
    assert_eq!(unsafe {libc::fcntl(read.as_raw_fd(),libc::F_GET_SEALS)} & super::super::super::SEALS, super::super::super::SEALS);
    assert_eq!(unsafe {libc::pwrite(witness.writer.as_ref().unwrap().as_raw_fd(),b"x".as_ptr().cast(),1,0)}, -1);
    assert!(witness.build(b"replacement DATA").is_err());
    assert!(witness.writer.is_some() && witness.read.is_some());
}

#[test]
fn oversized_witness_refuses_before_creation_without_issuing_any_owner() {
    let mut witness = WitnessFile::staged();
    assert!(witness.build(&vec![b'x';LIMIT+1]).is_err());
    assert!(witness.writer.is_none() && witness.read.is_none());
}

#[test]
fn regular_file_or_writable_memfd_is_not_a_native_export() {
    let file: OwnedFd=File::open("/dev/null").unwrap().into();
    assert!(read_native_witness(&file).is_err());
    let mut witness=WitnessFile::staged();witness.build(b"not an export").unwrap();
    let writable:OwnedFd=witness.writer.as_ref().unwrap().try_clone().unwrap().into();
    assert!(read_native_witness(&writable).is_err());
    assert!(unsafe{libc::fcntl(file.as_raw_fd(),libc::F_GETFD)}>=0);
}

#[test]
fn malformed_original_fd_stays_in_root_carrier_without_published_state() {
    let mut witness=WitnessFile::staged();witness.build(b"not qualification JSON").unwrap();
    let root=RootQualification::staged(witness.fd().unwrap().try_clone().unwrap());
    if let Ok(raw)=read_native_witness(&root.original_fd){assert!(serde_json::from_slice::<NativeExportData>(&raw).is_err());}
    assert!(root.export.is_none()&&root.request.is_none()&&root.ack.is_none()&&!root.published);
    assert!(unsafe{libc::fcntl(root.original_fd.as_raw_fd(),libc::F_GETFD)}>=0);
}

#[test]
fn duplicate_original_export_retains_both_descriptors_and_closes_admission() {
    let first:OwnedFd=File::open("/dev/null").unwrap().into();let n=first.as_raw_fd();
    let second:OwnedFd=File::open("/dev/null").unwrap().into();let m=second.as_raw_fd();
    let mut root=RootQualification::staged(first);assert!(root.duplicate(second).is_err());
    assert!(root.closed()&&!root.published&&root.duplicates.len()==1);
    assert!(unsafe{libc::fcntl(n,libc::F_GETFD)}>=0&&unsafe{libc::fcntl(m,libc::F_GETFD)}>=0);
}

#[test]
fn real_original_gate_rejects_foreign_export_credentials_and_keeps_fd() {
    let (a,b)=wire::runtime_socket_pair().unwrap();let sender=storage::Endpoint::capture(a).unwrap();
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let data=super::super::super::BootstrapData{version:1,nonce:uuid::Uuid::new_v4().to_string(),constraint:original.constraint()};
    let own=Process::current().unwrap();
    let mut gate=super::super::super::Gate::capture(b,own.fd().try_clone().unwrap(),unsafe{libc::geteuid()}^1,data.clone(),original).unwrap();
    let frame=super::super::super::Frame{version:1,serial:1,nonce:data.nonce,constraint:data.constraint,
        message:super::super::super::Message::RuntimeImageQualificationExport};
    let fd:OwnedFd=File::open("/dev/null").unwrap().into();
    assert!(sender.send(&storage::encode(&frame,&[]).unwrap(),Some(&fd)).unwrap());
    assert!(gate.poll().is_err());let packet=gate.pending_packet.as_ref().unwrap();
    assert_eq!(packet.fds.len(),1);assert!(unsafe{libc::fcntl(packet.fds[0].as_raw_fd(),libc::F_GETFD)}>=0);
}

#[test]
fn pending_native_bootstrap_has_no_export_or_qualification_owner() {
    let bootstrap=super::super::super::ManagedMachineBootstrap::empty();
    assert!(bootstrap.runtime_qualification.is_none()&&bootstrap.runtime_ready_observation.is_none()&&bootstrap.runtime_client.is_none());
}
