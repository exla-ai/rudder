//! Local codec/replay/actual socket tests, not authenticated Root proof issuers.
//! Full Root mount/cross-UID capacity qualification remains a disposable-VM gate.
use super::*;
fn budget()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn request(serial:u64,slot:uuid::Uuid,retirement:uuid::Uuid,budget:&FinalizationBudget)->Vec<u8> {
    encode(&Envelope {version:1,serial,constraint:budget.constraint(),request:Request::RetireSeed {
        slot,retirement,readers:Vec::new()}},&[]).unwrap()
}
#[test]
fn original_retirement_packet_replay_is_byte_exact_and_never_a_new_effect_or_deadline() {
    let original=budget();let slot=uuid::Uuid::new_v4();let operation=uuid::Uuid::new_v4();
    let bytes=request(7,slot,operation,&original);
    // This transport/codec fixture has NO actual Root client/server owner and
    // cannot authenticate/mint a RootSeedRetired or native actor-drain receipt.
    let response=encode(&Answer {version:1,serial:7,reply:Reply::RetirementPending {slot,retirement:operation}},&[]).unwrap();
    let held=RetirementAnswer {serial:7,digest:Sha256::digest(&bytes).to_vec(),bytes:response.clone(),_retired:None};
    assert_eq!(held.replay(7,&bytes,false).unwrap(),response);
    assert!(held.replay(8,&bytes,false).is_err());assert!(held.replay(7,&bytes,true).is_err());
    assert!(held.replay(7,&request(7,slot,uuid::Uuid::new_v4(),&original),false).is_err());
    assert!(held.replay(7,&request(7,slot,operation,&FinalizationBudget::begin(Duration::from_secs(29)).unwrap()),false).is_err());
    // Returning the retained old answer needs no new operation/budget. This is
    // past-reply transport only; no unmount/unpin/release is attempted here.
    original.cancel();assert!(original.remaining().is_err());assert_eq!(held.replay(7,&bytes,false).unwrap(),response);
}
#[test]
fn serialized_drained_flag_or_extra_path_can_never_issue_native_join_authority() {
    let budget=budget();let slot=uuid::Uuid::new_v4();let operation=uuid::Uuid::new_v4();
    for extra in ["drained","mount_path","joined_receipt"] {
        let mut value=serde_json::to_value(Envelope {version:1,serial:1,constraint:budget.constraint(),
            request:Request::RetireSeed {slot,retirement:operation,readers:Vec::new()}}).unwrap();
        value["request"][extra]=serde_json::json!(true);
        assert!(decode::<Envelope>(&encode(&value,&[]).unwrap()).is_err());
    }
}
#[test]
fn actual_nonblocking_seqpacket_backpressure_preserves_exact_held_reply_for_later_delivery() {
    let mut fds=[-1;2];assert_eq!(unsafe {libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,fds.as_mut_ptr())},0);
    let sender=Endpoint::capture(unsafe {OwnedFd::from_raw_fd(fds[0])}).unwrap();
    let receiver=Endpoint::capture(unsafe {OwnedFd::from_raw_fd(fds[1])}).unwrap();
    let dummy=vec![b'x';1024];let mut count=0;
    while sender.send(&dummy,None).unwrap(){count+=1;assert!(count<4096,"kernel queue must remain bounded");}
    let mut held=Some(b"retained-original-ack".to_vec());
    assert!(!sender.send(held.as_ref().unwrap(),None).unwrap());assert!(held.is_some());
    for _ in 0..count {assert_eq!(receiver.receive().unwrap().unwrap().bytes,dummy);}
    assert!(sender.send(held.as_ref().unwrap(),None).unwrap());held=None;
    assert_eq!(receiver.receive().unwrap().unwrap().bytes,b"retained-original-ack");assert!(held.is_none());
}
