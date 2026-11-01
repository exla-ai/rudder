//! Actual kernel/FD component fixtures. No fake Root grant, registration,
//! source owner, scheduler readiness or normal retirement receipt is issued.
use super::*;
use std::{io::Write,process::{Command,Stdio},os::unix::process::CommandExt};
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn binding()->ManagedExecutionBinding{ManagedExecutionBinding{format_version:2,request_id:Uuid::new_v4().to_string(),run_id:"fixture".into(),run_epoch:1,execution_id:Uuid::new_v4().to_string(),operation_id:Uuid::new_v4().to_string(),view_id:Uuid::new_v4().to_string(),owner:crate::protocol::OwnerEpoch{owner:"fixture-only".into(),epoch:1},execution_profile:"fixture".into(),frontend_start:ManagedFrontendStart::Immediate,server_command:None,guest_socket:"/tmp/fixture.sock".into(),profile:None}}
fn capsule(b:&ManagedExecutionBinding)->RuntimeImageCapsuleData{
    let hash="1".repeat(64);let floor=RuntimeFdData{device:1,inode:2,mount_id:3,directory:true};
    let selected_executables=["package/dist/native/rudder-native","package/dist/native/rudder-jj","usr/local/bin/rudder-root-storage","usr/local/bin/rudder-machine-root"].into_iter().map(|p|RuntimeExecutableData{installer_path:p.into(),sha256:hash.clone(),size_bytes:1}).collect();
    RuntimeImageCapsuleData{version:1,request_nonce:Uuid::new_v4().to_string(),binding_sha256:hex::encode(Sha256::digest(canonical_json(b).unwrap())),engine_instance_id:Uuid::new_v4().to_string(),epoch:"1".into(),config_sha256:hash.clone(),registration_proof_sha256:hash.clone(),boot_id:Uuid::new_v4().to_string(),launcher_pid:std::process::id(),service_pid:std::process::id(),engine_pid:std::process::id(),helper_pid:std::process::id(),helper_start_ticks:"1".into(),launcher_sha256:hash.clone(),service_sha256:hash.clone(),engine_sha256:hash,
        inventory:ObjectRef::from_bytes(crate::protocol::ObjectKind::Blob,b"untrusted inventory DATA"),manifest:ObjectRef::from_bytes(crate::protocol::ObjectKind::Blob,b"untrusted manifest DATA"),installer_input:ObjectRef::from_bytes(crate::protocol::ObjectKind::Directory,b"untrusted installer DATA"),constraint:budget().constraint(),floor:floor.clone(),components:COMPONENTS.iter().map(|(_,_,_,d)|RuntimeFdData{directory:*d,..floor.clone()}).collect(),selected_executables}
}
#[test]
fn actual_seqpacket_transfers_original_fd_credentials_and_cloexec(){
    let(a,b)=runtime_socket_pair().unwrap();let a=RuntimeEndpoint::capture(a).unwrap();let b=RuntimeEndpoint::capture(b).unwrap();let mut file=tempfile::tempfile().unwrap();file.write_all(b"actual retained file").unwrap();let fd:OwnedFd=file.into();
    assert!(a.send_data(b"component-only",&[&fd]).unwrap());let packet=b.receive_data().unwrap().unwrap();assert_eq!(packet.credentials.pid,std::process::id()as i32);assert_eq!(packet.credentials.uid,unsafe{libc::geteuid()});assert_eq!(packet.fds.len(),1);
    assert_eq!(fd_data(&fd).unwrap(),fd_data(&packet.fds[0]).unwrap());assert_ne!(unsafe{libc::fcntl(packet.fds[0].as_raw_fd(),libc::F_GETFD)}&libc::FD_CLOEXEC,0);
    let mut bytes=[0;20];assert_eq!(File::from(packet.fds[0].try_clone().unwrap()).read_at(&mut bytes,0).unwrap(),20);assert_eq!(&bytes,b"actual retained file");
}
#[test]
fn actual_nonblocking_eagain_retains_original_transport_and_fd(){
    let(a,b)=runtime_socket_pair().unwrap();let a=RuntimeEndpoint::capture(a).unwrap();let b=RuntimeEndpoint::capture(b).unwrap();let file:OwnedFd=tempfile::tempfile().unwrap().into();let mut blocked=false;
    for _ in 0..4096{if !a.send_data(b"bounded queue DATA",&[&file]).unwrap(){blocked=true;break}}
    assert!(blocked);assert!(unsafe{libc::fcntl(file.as_raw_fd(),libc::F_GETFD)}>=0);let packet=b.receive_data().unwrap().unwrap();assert_eq!(fd_data(&packet.fds[0]).unwrap(),fd_data(&file).unwrap());
}
#[test]
fn received_untrusted_data_cannot_mint_root_grant_and_keeps_received_fds(){
    let(a,b)=runtime_socket_pair().unwrap();let a=RuntimeEndpoint::capture(a).unwrap();let b=RuntimeEndpoint::capture(b).unwrap();let binding=binding();let data=capsule(&binding);data.validate_data().unwrap();
    let fds:Vec<OwnedFd>=(0..MAX_RUNTIME_FDS).map(|_|tempfile::tempfile().unwrap().into()).collect();let refs:Vec<_>=fds.iter().collect();assert!(a.send_data(&canonical_json(&data).unwrap(),&refs).unwrap());let mut owner=None;let mut pending=None;
    assert!(RuntimeImageGrant::receive_retaining(&mut owner,&mut pending,&b,&binding).is_err());assert!(owner.is_none());let p=pending.as_ref().unwrap();assert_eq!(p.fds.len(),MAX_RUNTIME_FDS);
    assert!(p.fds.iter().all(|fd|unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)}>=0));
}
#[test]
fn held_component_fd_does_not_adopt_replacement_path(){
    let root=tempfile::tempdir().unwrap();let path=root.path().join("component");fs::write(&path,b"original").unwrap();let original:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&path).unwrap().into();let captured=fd_data(&original).unwrap();
    fs::rename(&path,root.path().join("prior")).unwrap();fs::write(&path,b"replacement").unwrap();let replacement:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&path).unwrap().into();
    assert_eq!(fd_data(&original).unwrap(),captured);assert_ne!(fd_data(&replacement).unwrap(),captured);
}
#[test]
fn foreign_mount_role_and_selected_elf_mismatch_refuse_data_admission(){
    let mut data=capsule(&binding());data.validate_data().unwrap();data.components[0].mount_id+=1;assert!(data.validate_data().is_err());
    let mut data=capsule(&binding());data.selected_executables[0].sha256="2".repeat(64);assert!(data.validate_data().is_err());
    let mut data=capsule(&binding());data.components[0].directory=false;assert!(data.validate_data().is_err());
}
#[test]
fn actual_kernel_peer_pidfd_or_explicit_unsupported_refusal(){
    let(a,_b)=runtime_socket_pair().unwrap();match original_peer_pidfd(a.as_raw_fd()){
        Ok(peer)=>{assert_eq!(pid_from_fd(&peer).unwrap(),std::process::id());assert_ne!(unsafe{libc::fcntl(peer.as_raw_fd(),libc::F_GETFD)}&libc::FD_CLOEXEC,0);},
        Err(e)=>assert!(e.to_string().contains("SO_PEERPIDFD unavailable")),
    }
}
#[derive(Serialize,Deserialize)]
struct ChildRoute {name:String,parent:u32,device:u64,inode:u64,constraint:FinalizationConstraint}
const ROUTE:&str="RUDDER_RUNTIME_FD_COMPONENT_ROUTE";
#[test]
#[ignore="subprocess fixture entry only; parent supplies bounded benign DATA"]
fn runtime_exec_fd_receiver_fixture(){
    let route:ChildRoute=serde_json::from_str(&std::env::var(ROUTE).expect("fixture routing required")).unwrap();let original=FinalizationBudget::from_constraint(&route.constraint).unwrap();
    // All harmless transfer FDs were CLOEXEC; no inherited payload FD exists
    // during the exec-to-ND0 interval. Routing env alone conveys no authority.
    for raw in 3..64{let mut m:libc::stat=unsafe{std::mem::zeroed()};if unsafe{libc::fstat(raw,&mut m)}==0{assert_ne!((m.st_dev,m.st_ino),(route.device,route.inode));}}
    assert_eq!(unsafe{libc::prctl(libc::PR_SET_DUMPABLE,0,0,0,0)},0);assert_eq!(unsafe{libc::prctl(libc::PR_GET_DUMPABLE,0,0,0,0)},0);
    let raw=unsafe{libc::socket(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0)};assert!(raw>=0);let fd=unsafe{OwnedFd::from_raw_fd(raw)};let yes=1i32;assert_eq!(unsafe{libc::setsockopt(raw,libc::SOL_SOCKET,libc::SO_PASSCRED,(&yes as *const i32).cast(),std::mem::size_of_val(&yes)as _)},0);let(address,len)=abstract_address(&route.name).unwrap();assert_eq!(unsafe{libc::connect(raw,(&address as *const libc::sockaddr_un).cast(),len)},0);
    assert_eq!(peer_credentials(raw).unwrap().pid,route.parent as i32);let peer=original_peer_pidfd(raw).unwrap();assert_eq!(pid_from_fd(&peer).unwrap(),route.parent);let endpoint=RuntimeEndpoint::capture(fd).unwrap();
    let packet=loop{original.remaining().unwrap();if let Some(p)=endpoint.receive_data().unwrap(){break p}std::thread::sleep(original.phase_limit(Duration::from_millis(2)).unwrap());};
    assert_eq!(packet.bytes,b"benign FD transport only");assert_eq!(packet.fds.len(),MAX_RUNTIME_FDS);assert_eq!(packet.credentials.pid,route.parent as i32);
    for fd in packet.fds{let m=File::from(fd.try_clone().unwrap()).metadata().unwrap();assert_eq!((m.dev(),m.ino()),(route.device,route.inode));let mut bytes=[0;6];assert_eq!(File::from(fd).read_at(&mut bytes,0).unwrap(),6);assert_eq!(&bytes,b"benign");}
}
#[test]
fn actual_post_exec_fd_transfer_uses_original_child_and_new_protected_connection(){
    if unsafe{libc::geteuid()}!=1500{return}let(a,_b)=runtime_socket_pair().unwrap();if original_peer_pidfd(a.as_raw_fd()).is_err(){return}
    let original=budget();let mut source=tempfile::tempfile().unwrap();source.write_all(b"benign").unwrap();let m=source.metadata().unwrap();let source:OwnedFd=source.into();
    let fds=(0..MAX_RUNTIME_FDS).map(|_|source.try_clone().unwrap()).collect();let mut handoff=None;RuntimeExecHandoff::capture_transport_retaining(&mut handoff,original.clone(),b"benign FD transport only".to_vec(),fds).unwrap();
    let route=ChildRoute{name:handoff.as_ref().unwrap().name_data().into(),parent:std::process::id(),device:m.dev(),inode:m.ino(),constraint:original.constraint()};
    let mut child=Command::new(std::env::current_exe().unwrap()).args(["--exact","execution::runtime_image::tests::runtime_exec_fd_receiver_fixture","--ignored","--nocapture","--test-threads=1"]).env_clear().env(ROUTE,serde_json::to_string(&route).unwrap()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let childfd=super::super::handoff::pidfd(child.id()).unwrap();let run=handoff.as_mut().unwrap().dispatch_original_child_retaining(childfd);
    if run.is_err(){let _=child.kill();let _=child.wait();}run.unwrap();let status=loop{if let Some(status)=child.try_wait().unwrap(){break status}if original.remaining().is_err(){let _=child.kill();let _=child.wait();panic!("actual fixture child failed to settle under original clock")}std::thread::sleep(Duration::from_millis(2));};
    assert!(status.success());assert!(handoff.as_ref().unwrap().sent);assert!(!handoff.as_ref().unwrap().unknown);assert!(pid_from_fd(handoff.as_ref().unwrap().child.as_ref().unwrap()).is_err());
}
#[test]
fn cancellation_before_exec_dispatch_retains_actual_carrier_without_new_clock(){
    let original=budget();let source:OwnedFd=tempfile::tempfile().unwrap().into();let fds=(0..MAX_RUNTIME_FDS).map(|_|source.try_clone().unwrap()).collect();let mut handoff=None;
    RuntimeExecHandoff::capture_transport_retaining(&mut handoff,original.clone(),b"bounded benign DATA".to_vec(),fds).unwrap();let prior=handoff.as_ref().unwrap().original.constraint();original.cancel();
    assert!(handoff.as_mut().unwrap().dispatch_original_child_retaining(super::super::handoff::pidfd(std::process::id()).unwrap()).is_err());let state=handoff.as_ref().unwrap();assert_eq!(state.original.constraint(),prior);assert!(state.listener.is_some()&&state.child.is_some());assert_eq!(state.fds.len(),MAX_RUNTIME_FDS);assert!(!state.sent);
}
