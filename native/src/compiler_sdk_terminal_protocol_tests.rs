//! DATA/schema and actual socket components. No Root/SDK owner is fabricated.
use super::*;
use rudder_engine::execution::runtime_image::RuntimeDisposalRead;
fn request()->FailureCloseRequest{let config="a".repeat(64);let clock=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();FailureCloseRequest{version:1,kind:"compiler_sdk_failed_job_close_v1".into(),
    binding:JobBinding{serial:1,nonce:dispatch_nonce("engine","epoch",&config,1).unwrap(),engine_instance_id:"engine".into(),epoch:"epoch".into(),config_sha256:config,
        source_root:ObjectRef::from_bytes(ObjectKind::SourceRoot,b"DATA only source"),action_key:ObjectRef::from_bytes(ObjectKind::ActionKey,b"DATA only action"),build_constraint:clock.constraint()},
    helper:HelperFailureData{source_kind:"helper_failure".into(),original_helper_pid:42,original_helper_start:"123".into(),exit_code:Some(7),signal:None,stdout_eof:true,stderr_eof:true,stdout_bytes:0,stderr_bytes:7,
        stdout_sha256:digest(b""),stderr_sha256:digest(b"failure"),stderr_diagnostic:"failure".into()},cleanup_constraint:FinalizationBudget::begin(Duration::from_secs(5)).unwrap().constraint()}}
#[test]fn terminal_helper_failure_data_never_accepts_success_or_incomplete_eof_or_oversize_diagnostic(){let actual=request();actual.validate().unwrap();
    let mut bad=actual.clone();bad.helper.exit_code=Some(0);assert!(bad.validate().is_err());
    bad=actual.clone();bad.helper.signal=Some(9);assert!(bad.validate().is_err());
    bad=actual.clone();bad.helper.stderr_eof=false;assert!(bad.validate().is_err());
    bad=actual.clone();bad.helper.source_kind="compiler_failure".into();assert!(bad.validate().is_err());
    bad=actual.clone();bad.helper.stderr_diagnostic="x".repeat(4097);assert!(bad.validate().is_err());
    bad=actual;bad.helper.stdout_sha256="unverified".into();assert!(bad.validate().is_err());}
#[test]fn terminal_failure_close_binds_original_observation_source_job_and_cleanup_clock(){let actual=request();let ack=FailureCloseAck{version:1,kind:"compiler_sdk_failed_job_resources_closed_v1".into(),request:actual.clone(),original_helper_pid:42,original_helper_start:"123".into()};ack.validate(&actual).unwrap();
    let mut changed=actual.clone();changed.helper.stderr_diagnostic="different original stderr".into();assert!(ack.validate(&changed).is_err());
    changed=actual.clone();changed.binding.source_root=ObjectRef::from_bytes(ObjectKind::SourceRoot,b"other source");assert!(ack.validate(&changed).is_err());
    changed=actual.clone();changed.cleanup_constraint=FinalizationBudget::begin(Duration::from_secs(3)).unwrap().constraint();assert!(ack.validate(&changed).is_err());
    changed=actual.clone();changed.binding.serial=2;assert!(ack.validate(&changed).is_err());
    // A failure frame cannot be parsed as the successful artifact close format.
    let raw=canonical_json(&actual).unwrap();assert!(serde_json::from_slice::<CloseRequest>(&raw).is_err());
    let mut json=serde_json::to_value(actual).unwrap();json["artifact_result"]=serde_json::to_value(ObjectRef::from_bytes(ObjectKind::ActionResult,b"invented artifact")).unwrap();assert!(serde_json::from_value::<FailureCloseRequest>(json).is_err());}
#[test]fn failure_close_packet_and_half_close_cannot_hide_an_actual_peer_alias(){let(left,right)=wire::runtime_socket_pair().unwrap();let alias=right.try_clone().unwrap();
    let left=RuntimeEndpoint::capture(left).unwrap();let right=RuntimeEndpoint::capture(right).unwrap();let data=request();let raw=canonical_json(&data).unwrap();assert!(right.send_data(&raw,&[]).unwrap());right.shutdown_disposal_write().unwrap();
    let mut held=None;assert!(matches!(left.receive_disposal_retaining(&mut held).unwrap(),RuntimeDisposalRead::Packet));let packet=held.as_ref().unwrap();
    let observed:FailureCloseRequest=serde_json::from_slice(&packet.bytes).unwrap();assert_eq!(observed,data);assert_eq!(packet.credentials.pid,std::process::id()as i32);assert!(packet.fds.is_empty());
    held.take();drop(right);assert!(matches!(left.receive_disposal_retaining(&mut held).unwrap(),RuntimeDisposalRead::ReadClosed{peer_fully_closed:false}));
    drop(alias);assert!(matches!(left.receive_disposal_retaining(&mut held).unwrap(),RuntimeDisposalRead::ReadClosed{peer_fully_closed:true}));}
