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

#[test]
fn sdk_ready_observations_share_original_qualification_window_without_renewal() {
    let original=FinalizationBudget::begin_sdk_qualification().unwrap();
    let constraint=original.constraint();let end=deadline(&constraint);
    let root=end-240_000_000_000;let native=root+90_000_000_000;
    original_receipt_window(&constraint,&root.to_string(),&native.to_string()).unwrap();
    assert!(original_receipt_window(&constraint,&root.to_string(),&end.to_string()).is_err());
    assert!(original_receipt_window(&constraint,&(root-1).to_string(),&native.to_string()).is_err());
    assert_eq!(original.constraint(),constraint);
}

#[test]
fn finite_sdk_ready_payloads_fit_sealed_evidence_bound_and_oversize_still_refuses() {
    use rudder_engine::{build::ToolchainIdentity,execution::compiler_sdk::{compiler_sdk_profiles_data,CompilerSdkSemanticData,CompilerSdkMountData}};
    // Schema/size DATA only: no Root registration, mount or ReadyOrigin is issued.
    for profile in compiler_sdk_profiles_data() {
        let catalog=profile.catalog_data();
        let (semantic,_)=CompilerSdkSemanticData::from_members_data(catalog.installer_input.clone(),catalog.artifact_manifest.clone(),catalog.artifact_manifest.size_bytes,0o555).unwrap();
        let mut toolchains=BTreeMap::new();
        for tool in profile.tools {
            let mut configuration=BTreeMap::from([("executable_raw_sha256".into(),"a".repeat(64))]);
            if let Some(entry)=tool.compiler_entrypoint{configuration.insert("compiler_entrypoint".into(),entry.into());}
            toolchains.insert(tool.name.into(),ToolchainIdentity{version:semantic.directory.digest.clone(),closure:semantic.directory.clone(),executable:catalog.artifact_manifest.clone(),executable_path:tool.relative_path.into(),configuration});
        }
        let mount=CompilerSdkMountData{catalog:catalog.clone(),semantic,floor:wire::RuntimeFdData{device:10,inode:11,mount_id:12,directory:true},toolchains};
        mount.validate_data().unwrap();
        let original=FinalizationBudget::begin_sdk_qualification().unwrap();let constraint=original.constraint();let end=deadline(&constraint);
        let step=|out:&[u8]|serde_json::json!({"exit_code":0,"stdout_sha256":digest(out),"stderr_sha256":digest(b""),"stdout_bytes":out.len(),"stderr_bytes":0});
        let workload=serde_json::json!({"format_version":1,"kind":"actual-fixed-rust-library-typescript-sdk-workload-v1","network_namespace":"net:[4294967295]","user_namespace":"user:[4294967295]","mount_namespace":"mnt:[4294967295]","pid_namespace":"pid:[4294967295]",
            "started_boottime_nanoseconds":(end-2).to_string(),"completed_boottime_nanoseconds":(end-1).to_string(),"sdk_python":{"device":"4294967295","inode":"18446744073709551615","sha256":"a".repeat(64),"size_bytes":2147483648u64},"sdk_root_device":"4294967295","sdk_root_readonly":true,
            "compiler_capabilities":{"CapEff":"0000000000000000","CapPrm":"0000000000000000","CapInh":"0000000000000000","CapAmb":"0000000000000000","CapBnd":"0000000000000000"},"buck2_static":true,
            "buck2":step(b"buck2 fixture DATA"),"rust":step(b""),"rust_result":step(b""),"typescript":step(b""),"typescript_result":step(b"rudder-sdk-typescript-ok\n"),"rust_artifact_sha256":"b".repeat(64),"typescript_artifact_sha256":"c".repeat(64),"models":false,"source_authority":false,"normal_retirement_authority":false});
        compiler_sdk::validate_workload(&workload,&constraint).unwrap();
        let nonce=uuid::Uuid::new_v4().to_string();let request_nonce=uuid::Uuid::new_v4().to_string();
        let mut evidence=ReadyEvidenceData{version:1,kind:"original-runtime-enable-ready-evidence-v1".into(),
            request:Enable{version:1,kind:"runtime_image_enable_v1".into(),bootstrap_nonce:nonce.clone(),request_nonce:request_nonce.clone(),constraint:constraint.clone()},
            ack:EnableAck{version:1,kind:"runtime_image_ready_v1".into(),bootstrap_nonce:nonce,request_nonce,engine_instance_id:uuid::Uuid::new_v4().to_string(),epoch:"18446744073709551615".into(),mount_id:u64::MAX,
                compiler_sdk:Some(compiler_sdk::CompilerSdkEnableData{mount,probe:Some(compiler_sdk::CompilerSdkProbeData{format_version:1,kind:"original-sdk-fixed-helper-complete-v1".into(),helper_pid:u32::MAX,helper_start_ticks:u64::MAX.to_string(),original_constraint:constraint,workload})}),
                inventory:catalog.inventory.clone(),manifest:catalog.manifest.clone(),installer_input:catalog.installer_input.clone(),selected_executables:["package/dist/native/rudder-native","package/dist/native/rudder-jj","usr/local/bin/rudder-root-storage","usr/local/bin/rudder-machine-root"].iter().map(|path|RuntimeExecutableData{installer_path:(*path).into(),sha256:"d".repeat(64),size_bytes:512*1024*1024}).collect()},
            service_pid:u32::MAX,service_start_ticks:u64::MAX.to_string(),registration_proof_sha256:"e".repeat(64),config_sha256:"f".repeat(64),boot_id:uuid::Uuid::new_v4().to_string(),root_ack_observed_boottime_nanoseconds:(end-1).to_string()};
        let bytes=encode_evidence(&evidence).unwrap();assert!(bytes.len()>4096,"fixture must expose the previous bound");
        let sealed=super::super::super::sealed_bytes(&bytes).unwrap();
        let read=if unsafe{libc::geteuid()}==0{super::super::super::read_sealed_bytes(&sealed,READY_LIMIT).unwrap()}
            else{let file=File::from(sealed.try_clone().unwrap());let mut read=vec![0;bytes.len()];file.read_exact_at(&mut read,0).unwrap();read};
        assert_eq!(read,bytes);let decoded:ReadyEvidenceData=serde_json::from_slice(&read).unwrap();assert_eq!(decoded.ack.compiler_sdk.unwrap().mount.toolchains.len(),profile.tools.len());
        evidence.ack.compiler_sdk.as_mut().unwrap().probe.as_mut().unwrap().workload["oversized_data"]=serde_json::Value::String("x".repeat(READY_LIMIT));assert!(encode_evidence(&evidence).is_err());
        evidence.ack.compiler_sdk=None;assert!(encode_evidence(&evidence).unwrap().len()<4096);
    }
}

#[test]
#[ignore="local Linux sealed-reader regression; requires enabled unprivileged user namespaces"]
fn local_sealed_reader_accepts_full_sdk_evidence_with_same_finite_bound() {
    // A nested namespace only supplies UID0 metadata for the real sealed DATA
    // reader. It cannot construct a genuine Root registration or ReadyOrigin.
    let pid=unsafe{libc::fork()};assert!(pid>=0);
    if pid==0{
        let result=std::panic::catch_unwind(||{
            let uid=unsafe{libc::getuid()};let gid=unsafe{libc::getgid()};
            assert_eq!(unsafe{libc::unshare(libc::CLONE_NEWUSER)},0);
            fs::write("/proc/self/setgroups",b"deny").unwrap();
            fs::write("/proc/self/uid_map",format!("0 {uid} 1")).unwrap();
            fs::write("/proc/self/gid_map",format!("0 {gid} 1")).unwrap();
            assert_eq!(unsafe{libc::geteuid()},0);
            finite_sdk_ready_payloads_fit_sealed_evidence_bound_and_oversize_still_refuses();
        });
        unsafe{libc::_exit(if result.is_ok(){0}else{1})}
    }
    let mut status=0;assert_eq!(unsafe{libc::waitpid(pid,&mut status,0)},pid);
    assert!(libc::WIFEXITED(status)&&libc::WEXITSTATUS(status)==0,"original sealed-reader child failed: {status}");
}
