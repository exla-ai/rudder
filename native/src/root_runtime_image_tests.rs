//! Mechanism/negative tests only. They never manufacture a successful Root
//! registration, image admission, helper drain or normal retirement receipt.
use super::*;
use std::io::Write;
use super::super::host_ingress::{HostImagePreseedPolicy,HostBakedRuntimePolicy};
use rudder_engine::protocol::{ObjectRef,ObjectKind};
#[test]
fn legacy_image_policy_omits_optional_runtime_profile_in_canonical_bytes(){
    let raw=serde_json::json!({"image":"registry.fly.io/rudder-workers-exla@sha256:".to_owned()+&"1".repeat(64),"image_receipt_sha256":"2".repeat(64),"full_manifest_sha256":"3".repeat(64)});
    let data:HostImagePreseedPolicy=serde_json::from_value(raw.clone()).unwrap();assert!(data.runtime_mount.is_none());assert_eq!(serde_json::to_value(data).unwrap(),raw);
}
#[test]
fn runtime_profile_has_no_path_or_boolean_authority_override(){
    let object=ObjectRef::from_bytes(ObjectKind::Blob,b"inventory fixture DATA");let valid=HostBakedRuntimePolicy{profile:"baked-cas-v1".into(),inventory:object.clone()};valid.validate().unwrap();
    let mut data=serde_json::to_value(&valid).unwrap();data["root_ready"]=serde_json::Value::Bool(true);assert!(serde_json::from_value::<HostBakedRuntimePolicy>(data).is_err());
    let mut invalid=valid.clone();invalid.profile="legacy-v1".into();assert!(invalid.validate().is_err());invalid=valid.clone();invalid.inventory.kind=ObjectKind::Directory;assert!(invalid.validate().is_err());
    invalid=valid.clone();invalid.inventory.size_bytes=16*1024*1024+1;assert!(invalid.validate().is_err());invalid=valid;invalid.inventory.size_bytes=0;assert!(invalid.validate().is_err());
}
#[test]
fn actual_original_eventfd_cancellation_is_shared_and_absorbing(){
    let original=cancel_fd().unwrap();let duplicate=original.try_clone().unwrap();check_cancel(&original).unwrap();check_cancel(&duplicate).unwrap();signal_cancel(&original).unwrap();
    assert!(check_cancel(&duplicate).is_err());assert!(check_cancel(&original).is_err());assert!(unsafe{libc::fcntl(original.as_raw_fd(),libc::F_GETFD)}>=0);
    // Reading a status result never consumes/reset the actual eventfd.
    assert!(check_cancel(&duplicate).is_err());
}
#[test]
fn a_regular_file_cannot_substitute_original_cancellation_fd(){let file:OwnedFd=std::fs::File::open("/dev/null").unwrap().into();assert!(check_cancel(&file).is_err());}
#[test]
fn request_deadline_survives_serialization_and_cancellation_never_mints_new_budget(){
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();let request=HelperRequest{version:1,request_nonce:uuid::Uuid::new_v4().to_string(),binding_sha256:"1".repeat(64),engine_instance_id:uuid::Uuid::new_v4().to_string(),epoch:"1".into(),constraint:original.constraint()};
    let raw=canonical_json(&request).unwrap();let received:HelperRequest=serde_json::from_slice(&raw).unwrap();assert_eq!(received.constraint,original.constraint());
    original.cancel();assert!(original.remaining().is_err());assert_eq!(request.constraint,received.constraint);
    // A separate decoded time restriction is DATA only: no public runtime
    // mount/client/registered-owner constructor accepts it as authority.
}
#[test]
fn actual_streaming_control_read_cancellation_keeps_original_file_owner(){
    let path=std::env::temp_dir().join(format!("rudder-runtime-control-test-{}",uuid::Uuid::new_v4()));
    let mut file=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&path).unwrap();file.write_all(b"public config DATA").unwrap();let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();let before=stamp(&file).unwrap();original.cancel();
    assert!(read_control(&file,32768,&original).is_err());assert_eq!(stamp(&file).unwrap(),before);assert!(unsafe{libc::fcntl(file.as_raw_fd(),libc::F_GETFD)}>=0);drop(file);fs::remove_file(path).unwrap();
}
#[test]
#[ignore="actual Root plus fixed installed production images; negative role qualification only"]
fn root_uid_and_namespace_alone_do_not_create_runtime_bootstrap_proof(){
    assert_eq!(unsafe{libc::geteuid()},0);let own=Process::current().unwrap();let(a,b)=storage::bootstrap_socket_pair().unwrap();let(c,d)=storage::bootstrap_socket_pair().unwrap();
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();let data=BootstrapData{version:1,nonce:uuid::Uuid::new_v4().to_string(),constraint:original.constraint()};
    let raw=vec![a,c,own.fd().try_clone().unwrap(),File::open("/proc/self/exe").unwrap().into(),super::super::sealed_data(&data).unwrap(),storage::namespace(None,"user",libc::CLONE_NEWUSER).unwrap().into(),storage::namespace(None,"mnt",libc::CLONE_NEWNS).unwrap().into(),storage::namespace(None,"pid",libc::CLONE_NEWPID).unwrap().into()];
    let originals:Vec<_>=raw.iter().map(|fd|fd.try_clone().unwrap()).collect();
    assert!(RuntimeBootstrapProof::capture(raw,&own).is_err());assert!(originals.iter().all(|fd|unsafe{libc::fcntl(fd.as_raw_fd(),libc::F_GETFD)}>=0));drop((b,d));
}
