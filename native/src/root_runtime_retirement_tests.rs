//! Real original socket frames only. These component tests exercise retained
//! ACK handling; they never fabricate a Root/runtime owner or stop receipt.
use super::*;
fn original_request()->NativeRetirement {
    let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
    let request=Request{version:1,kind:REQUEST.into(),nonce:uuid::Uuid::new_v4().to_string(),engine_instance_id:"component-only".into(),epoch:"1".into(),constraint:original.constraint()};
    NativeRetirement{bytes:canonical_json(&request).unwrap(),original,request,sent:true,send_unknown:false,pending:None,ready:false}
}
fn endpoints()->(RuntimeEndpoint,RuntimeEndpoint) {
    let(a,b)=wire::runtime_socket_pair().unwrap();(RuntimeEndpoint::capture(a).unwrap(),RuntimeEndpoint::capture(b).unwrap())
}
fn send_ack(sender:&RuntimeEndpoint,request:Request) {
    assert!(sender.send_data(&canonical_json(&Ack{version:1,kind:ACK.into(),request}).unwrap(),&[]).unwrap());
}
#[test]
fn runtime_retirement_eof_never_becomes_an_original_ack() {
    let(sender,receiver)=endpoints();let mut retained=original_request();drop(sender);
    assert!(retained.poll_ack(&receiver,std::process::id(),&AtomicBool::new(false)).is_err());
    assert!(!retained.ready&&retained.pending.is_none());assert!(retained.sent);
}
#[test]
fn runtime_retirement_unknown_send_never_consumes_a_reply() {
    let(sender,receiver)=endpoints();let mut retained=original_request();retained.send_unknown=true;
    send_ack(&sender,retained.request.clone());
    assert!(retained.poll_ack(&receiver,std::process::id(),&AtomicBool::new(false)).is_err());
    assert!(!retained.ready&&retained.pending.is_none()&&retained.send_unknown);
}
#[test]
fn runtime_retirement_foreign_nonce_packet_stays_retained() {
    let(sender,receiver)=endpoints();let mut retained=original_request();let mut foreign=retained.request.clone();foreign.nonce=uuid::Uuid::new_v4().to_string();send_ack(&sender,foreign);
    assert!(retained.poll_ack(&receiver,std::process::id(),&AtomicBool::new(false)).is_err());
    assert!(!retained.ready&&retained.pending.is_some());
    // A later correct packet cannot replace the uncertain original observation.
    send_ack(&sender,retained.request.clone());
    assert!(retained.poll_ack(&receiver,std::process::id(),&AtomicBool::new(false)).is_err());assert!(!retained.ready);
}
#[test]
fn runtime_retirement_actual_channel_credentials_must_match_original_service() {
    let(sender,receiver)=endpoints();let mut retained=original_request();send_ack(&sender,retained.request.clone());
    assert!(retained.poll_ack(&receiver,std::process::id().checked_add(1).unwrap(),&AtomicBool::new(false)).is_err());
    assert!(!retained.ready&&retained.pending.is_some());
}
#[test]
fn runtime_retirement_late_exact_ack_keeps_original_budget_and_packet() {
    let(sender,receiver)=endpoints();let mut retained=original_request();send_ack(&sender,retained.request.clone());retained.original.cancel();
    assert!(retained.poll_ack(&receiver,std::process::id(),&AtomicBool::new(false)).is_err());
    assert!(!retained.ready&&retained.pending.is_some());assert!(retained.original.remaining().is_err());
}
