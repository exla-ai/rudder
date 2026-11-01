//! No Root/seed/join capability constructor. Codec bounds and actual retained
//! eventfd/observer custody only; the FUSE+UID fixture is separate and ignored.
use super::*;
#[test]
fn archive_capacity_reserves_all_live_and_unknown_cows_before_creation(){
    assert!(paged::archive_capacity_available(4094,[1,0]));
    assert!(!paged::archive_capacity_available(4095,[1,0]));
    assert!(!paged::archive_capacity_available(4095,[1,1]));
    // Retiring one seed transfers its reservation to archives unchanged.
    assert!(!paged::archive_capacity_available(4096,[0,0]));
    assert!(!paged::archive_capacity_available(usize::MAX,[1]));
    assert!(paged::archive_capacity_available(0,[256,256]));
}
#[test]
fn original_paged_inventory_header_fits_and_manifest_is_not_flat(){
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();let slot=uuid::Uuid::new_v4();
    let references=(0..16).map(|i|ObjectRef::from_bytes(ObjectKind::Blob,format!("object-{i}").as_bytes())).collect::<Vec<_>>();
    let frame=encode(&Envelope{version:1,serial:1,constraint:original.constraint(),request:Request::PagedInventory{slot,references:references.clone()}},&[]).unwrap();
    assert!(frame.len()<=4096);let(decoded,payload):(Envelope,_)=decode(&frame).unwrap();assert!(payload.is_empty());
    match decoded.request{Request::PagedInventory{slot:actual,references:actual_refs}=>{assert_eq!(actual,slot);assert_eq!(actual_refs,references)},_=>panic!("original frame changed")};
    let too_many=(0..64).map(|i|ObjectRef::from_bytes(ObjectKind::Blob,format!("object-{i}").as_bytes())).collect();
    assert!(encode(&Envelope{version:1,serial:2,constraint:original.constraint(),request:Request::PagedInventory{slot,references:too_many}},&[]).is_err());
}
#[test]
fn v2_bound_accepts_actual_four_mib_chunks_and_refuses_oversized_or_foreign_kinds(){
    let chunk=ObjectRef::from_bytes(ObjectKind::Blob,&vec![17;4<<20]);assert!(paged::bound(&chunk,chunk.size_bytes).is_ok());
    assert!(paged::bound(&chunk,chunk.size_bytes-1).is_err());let large=ObjectRef::from_bytes(ObjectKind::Blob,&vec![17;(4<<20)+1]);assert!(paged::bound(&large,large.size_bytes).is_err());
    let foreign=ObjectRef::from_bytes(ObjectKind::SourceRoot,b"{}");assert!(paged::bound(&foreign,foreign.size_bytes).is_err());
}
#[test]
fn actual_original_cancel_event_is_observed_and_joined_before_success(){
    let event=eventfd().unwrap();let actual=event.try_clone().unwrap();let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
    let mut cancellation=Cancellation::capture(actual).unwrap();cancellation.start(original.clone()).unwrap();cancel_event(&event);
    let end=std::time::Instant::now()+Duration::from_secs(3);
    while !cancellation.flag.load(Ordering::Acquire){assert!(std::time::Instant::now()<end);std::thread::sleep(Duration::from_millis(1));}
    cancellation.finish().unwrap();assert!(cancellation.result.as_ref().unwrap().is_ok());assert!(cancellation.check_success(&original).is_err());
}
#[test]
fn cancel_after_observer_join_still_refuses_ready_linearization(){
    let event=eventfd().unwrap();let mut cancellation=Cancellation::capture(event.try_clone().unwrap()).unwrap();
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();cancellation.start(original.clone()).unwrap();cancellation.finish().unwrap();
    assert!(cancellation.check_success(&original).is_ok());cancel_event(&event);
    // Deterministic late event: no timer, synthetic clock, Root ACK or new budget.
    assert!(cancellation.check_success(&original).is_err());
}
#[test]
fn unrelated_cancel_event_cannot_cancel_the_original_operation(){
    let event=eventfd().unwrap();let unrelated=eventfd().unwrap();let mut cancellation=Cancellation::capture(event.try_clone().unwrap()).unwrap();
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();cancellation.start(original.clone()).unwrap();cancel_event(&unrelated);
    cancellation.finish().unwrap();assert!(cancellation.check_success(&original).is_ok());
}
#[test]
fn original_cancel_descriptor_survives_actual_seqpacket_transfer(){
    let mut raw=[-1;2];assert_eq!(unsafe{libc::socketpair(libc::AF_UNIX,libc::SOCK_SEQPACKET|libc::SOCK_NONBLOCK|libc::SOCK_CLOEXEC,0,raw.as_mut_ptr())},0);
    let first=unsafe{OwnedFd::from_raw_fd(raw[0])};let second=unsafe{OwnedFd::from_raw_fd(raw[1])};
    for fd in [&first,&second]{let yes=1i32;assert_eq!(unsafe{libc::setsockopt(fd.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PASSCRED,(&yes as *const i32).cast(),std::mem::size_of::<i32>()as _)},0);}
    let first=Endpoint::capture(first).unwrap();let second=Endpoint::capture(second).unwrap();
    let actual_event=eventfd().unwrap();assert!(first.send(b"component-event-only",Some(&actual_event)).unwrap());
    let mut packet=second.receive().unwrap().unwrap();assert_eq!(packet.bytes,b"component-event-only");assert_eq!(packet.fds.len(),1);
    assert_eq!(packet.credentials.pid,std::process::id()as i32);assert_eq!(packet.credentials.uid,unsafe{libc::geteuid()});
    // This is only real FD transport: no Root sender or ready ACK is fabricated.
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();let mut cancellation=Cancellation::capture(packet.fds.pop().unwrap()).unwrap();
    cancel_event(&actual_event);cancellation.start(original.clone()).unwrap();
    let end=std::time::Instant::now()+Duration::from_secs(3);while !cancellation.flag.load(Ordering::Acquire){assert!(std::time::Instant::now()<end);std::thread::sleep(Duration::from_millis(1));}
    cancellation.finish().unwrap();assert!(cancellation.check_success(&original).is_err());
}
