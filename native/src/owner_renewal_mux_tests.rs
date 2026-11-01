//! Kernel DATA demux tests. No real PG/source/clock qualification is asserted.
use super::*;
fn registration_data()->RegistrationAck {serde_json::from_str(include_str!("../tests/fixtures/owner_mux_registration_data.json")).unwrap()}
fn exchange(operation:EngineOperationKind)->NativeExchange {let original=FinalizationBudget::begin(std::time::Duration::from_secs(1)).unwrap();let request=NativeRequest {version:1,request_nonce:uuid::Uuid::new_v4().to_string(),command_key:uuid::Uuid::new_v4().to_string(),operation,constraint:original.constraint().into()};NativeExchange {wire:bytes(&request).unwrap(),request,sent:true,received:None,ack:None,validated:false,uncertain:false}}
fn ack(request:&NativeRequest,r:&RegistrationAck)->EngineOperationAck {let b=&r.binding;EngineOperationAck {version:1,kind:"engine_operation_ack".into(),account_id:b.account_id.clone(),home_id:b.home_id.clone(),store_id:b.store_id.clone(),incarnation:b.incarnation.clone(),engine_instance_id:r.engine_instance_id.clone(),boot_id:r.boot_id.clone(),credential_id:r.credential_id.clone(),epoch:b.epoch.clone(),root_nonce:r.root_nonce.clone(),registration_proof_sha256:r.proof_sha256.clone(),request_nonce:request.request_nonce.clone(),command_key:request.command_key.clone(),operation:request.operation.clone(),payload_sha256:hex(&Sha256::digest(bytes(&request.operation).unwrap())),constraint:request.constraint.clone(),request_sha256:"a".repeat(64),result:Some(serde_json::json!({"test_data_only":true})),error:None}}
#[test]
fn regular_and_renewal_slots_keep_original_nonce_and_unknown_owner_separate() {
    // This test checks slot DATA only. Create an ordinary unprivileged pair;
    // the separate ignored test exercises production Root credential checks.
    use std::os::fd::FromRawFd;
    let mut raw=[-1;2];assert_eq!(unsafe{libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,raw.as_mut_ptr())},0);
    let (one,_two)=unsafe{(OwnedFd::from_raw_fd(raw[0]),OwnedFd::from_raw_fd(raw[1]))};let mut channel=NativeChannel::capture(one).unwrap();let mut regular=exchange(EngineOperationKind::PublishFreshRootsV1 {workspace_id:"one".into(),transfer_id:uuid::Uuid::new_v4().to_string()});regular.uncertain=true;let first=regular.request.request_nonce.clone();
    channel.active=Some(regular);let renewal=exchange(EngineOperationKind::RenewFreshOwnerV1 {workspace_id:"two".into(),owner_epoch:"2".into(),lease_id:uuid::Uuid::new_v4().to_string(),request_id:uuid::Uuid::new_v4().to_string(),expected_serial:"0".into()});let second=renewal.request.request_nonce.clone();channel.renewal=Some(renewal);
    assert!(channel.get(&first).unwrap().uncertain);assert_eq!(channel.take(&second).unwrap().request.request_nonce,second);assert!(channel.get(&first).unwrap().uncertain);assert!(channel.get(&second).is_none());assert!(channel.get("foreign").is_none());
}
#[test]
#[ignore="requires actual UID0 for production SCM credential checks; kernel DATA only, not mixed PG/S3 enablement"]
fn root_kernel_reply_demux_progresses_renewal_while_storage_reply_is_withheld() {
    assert_eq!(unsafe{libc::geteuid()},0);let r=registration_data();let (root,native)=storage::bootstrap_socket_pair().unwrap();let endpoint=storage::Endpoint::capture(root).unwrap();let mut channel=NativeChannel::capture(native).unwrap();
    channel.active=Some(exchange(EngineOperationKind::PublishFreshRootsV1 {workspace_id:"slow".into(),transfer_id:uuid::Uuid::new_v4().to_string()}));channel.renewal=Some(exchange(EngineOperationKind::RenewFreshOwnerV1 {workspace_id:"live".into(),owner_epoch:"2".into(),lease_id:uuid::Uuid::new_v4().to_string(),request_id:uuid::Uuid::new_v4().to_string(),expected_serial:"0".into()}));
    let first=channel.active.as_ref().unwrap().request.request_nonce.clone();let second=channel.renewal.as_ref().unwrap().request.request_nonce.clone();let response=ack(&channel.renewal.as_ref().unwrap().request,&r);assert!(endpoint.send(&bytes(&response).unwrap(),None).unwrap());
    EngineOperation::receive(&mut channel,std::process::id(),&r).unwrap();assert!(channel.get(&second).unwrap().validated);assert!(channel.get(&first).unwrap().received.is_none());
    let regular=ack(&channel.active.as_ref().unwrap().request,&r);assert!(endpoint.send(&bytes(&regular).unwrap(),None).unwrap());EngineOperation::receive(&mut channel,std::process::id(),&r).unwrap();assert!(channel.get(&first).unwrap().validated);assert!(channel.get(&second).unwrap().validated);
}
